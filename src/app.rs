use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use fs_err as fs;

use color_eyre::eyre::bail;
use dialoguer::{Input, MultiSelect, Select, theme::ColorfulTheme};
use fs::OpenOptions;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use iocraft::prelude::*;
use parking_lot::Mutex;
use re_tex::tex::Tex;
use ree_pak_core::{
    ExtractEvent, PakFile, pak::KnownAttr, read::entry::determine_extension_from_bytes,
    write::FileOptions,
};

use crate::{chunk::ChunkName, component::UpdateCheck, metadata::PakMetadata, util::human_bytes};

const AUTO_CHUNK_SELECTION_SIZE_THRESHOLD: usize = 1024 * 1024; // 1MB
const FALSE_TRUE_SELECTION: [&str; 2] = ["False", "True"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Automatic = 0,
    Manual = 1,
    Restore = 2,
}

impl Mode {
    fn from_index(index: usize) -> color_eyre::Result<Self> {
        match index {
            0 => Ok(Mode::Automatic),
            1 => Ok(Mode::Manual),
            2 => Ok(Mode::Restore),
            _ => bail!("Invalid mode index: {index}"),
        }
    }
}

#[derive(Clone)]
struct ChunkSelection {
    chunk_name: ChunkName,
    file_size: u64,
    full_path: PathBuf,
}

impl std::fmt::Display for ChunkSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.chunk_name, human_bytes(self.file_size))?;
        Ok(())
    }
}

pub struct App;

impl App {
    pub async fn run(&mut self) -> color_eyre::Result<()> {
        // Welcome message
        element! {
            View(
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                padding: Padding::Length(1),
                border_style: BorderStyle::Round,
                border_color: Color::Cyan,
            ) {
                Text(
                    content: "Monster Hunter: Wilds - Texture Decompressor",
                    color: Color::Green,
                    weight: Weight::Bold,
                    align: TextAlign::Center,
                )
                Text()
                Text(content: format!("Version v{} - Tool by @Eigeen", env!("CARGO_PKG_VERSION")))
                Text(content: "Repo: https://github.com/eigeen/mhws-tex-decompressor")
            }
        }
        .print();
        println!();

        // Check update (blocking)
        element! {
            UpdateCheck()
        }
        .render_loop()
        .await?;

        // Mode selection
        let mode = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Select mode")
            .items(["Automatic", "Manual", "Restore"])
            .default(0)
            .interact()?;
        let mode = Mode::from_index(mode)?;

        match mode {
            Mode::Automatic => self.auto_mode(),
            Mode::Manual => self.manual_mode(),
            Mode::Restore => self.restore_mode(),
        }
    }

    /// Scan for all pak files in the game directory, including DLC directory
    fn scan_all_pak_files(&self, game_dir: &Path) -> color_eyre::Result<Vec<ChunkSelection>> {
        let mut main_chunks = Vec::new();
        let mut dlc_chunks = Vec::new();

        // Scan main game directory
        self.scan_pak_files_in_dir(game_dir, &mut main_chunks)?;

        // Scan DLC directory if it exists
        let dlc_dir = game_dir.join("dlc");
        if dlc_dir.is_dir() {
            self.scan_pak_files_in_dir(&dlc_dir, &mut dlc_chunks)?;
        }

        // If both main and DLC have files, ask user which locations to process
        let selected_locations = if !main_chunks.is_empty() && !dlc_chunks.is_empty() {
            let locations = vec!["Main game directory", "DLC directory"];

            MultiSelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Select locations to process (Space to select, Enter to confirm)")
                .items(&locations)
                .defaults(&[true, true])
                .interact()?
        } else if !main_chunks.is_empty() {
            vec![0]
        } else if !dlc_chunks.is_empty() {
            vec![1]
        } else {
            vec![]
        };

        let mut all_chunks = Vec::new();
        for &location_idx in &selected_locations {
            match location_idx {
                0 => all_chunks.extend(main_chunks.iter().cloned()),
                1 => all_chunks.extend(dlc_chunks.iter().cloned()),
                _ => {}
            }
        }
        all_chunks.sort_by(|a, b| a.chunk_name.cmp(&b.chunk_name));

        Ok(all_chunks)
    }

    /// Scan pak files in a specific directory
    fn scan_pak_files_in_dir(
        &self,
        dir: &Path,
        all_chunks: &mut Vec<ChunkSelection>,
    ) -> color_eyre::Result<()> {
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();
            let file_path = entry.path();

            if !file_name.ends_with(".pak") {
                continue;
            }

            let chunk_name = match ChunkName::try_from_str(&file_name) {
                Ok(chunk_name) => chunk_name,
                Err(e) => {
                    println!("Invalid chunk name, skipped: {e}");
                    continue;
                }
            };

            let file_size = fs::metadata(&file_path)?.len();
            all_chunks.push(ChunkSelection {
                chunk_name,
                file_size,
                full_path: file_path,
            });
        }

        Ok(())
    }

    fn process_chunk(
        &self,
        input_path: &Path,
        output_path: &Path,
        use_full_package_mode: bool,
        use_feature_clone: bool,
    ) -> color_eyre::Result<()> {
        println!("Processing chunk: {}", input_path.display());

        let file = fs::File::open(input_path)?;
        let pak = PakFile::from_file(file.into())?;

        let total_entry_count = pak.metadata().entries().len();

        let tex_hashes = if use_full_package_mode {
            None
        } else {
            println!("Detecting TEX entries by file magic...");
            let scan_bar = ProgressBar::new(total_entry_count as u64);
            scan_bar.set_style(
                ProgressStyle::default_bar().template("Detecting TEX: {pos}/{len} {wide_bar}")?,
            );
            scan_bar.enable_steady_tick(Duration::from_millis(200));

            let mut hashes = HashSet::new();
            for entry in pak.metadata().entries() {
                let mut entry_reader = pak.open_entry(entry)?;
                let mut magic = [0u8; 8];
                if entry_reader.read_exact(&mut magic).is_ok()
                    && determine_extension_from_bytes(&magic) == Some("tex")
                {
                    hashes.insert(entry.hash());
                }
                scan_bar.inc(1);
            }
            scan_bar.finish_and_clear();
            println!("Detected {} TEX entries", hashes.len());
            Some(Arc::new(hashes))
        };

        let entry_count = if use_full_package_mode {
            total_entry_count
        } else {
            tex_hashes.as_ref().unwrap().len()
        };

        // new pak archive
        let out_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(output_path)?;
        // +1 for metadata
        let mut pak_writer =
            ree_pak_core::write::PakWriter::new(out_file, (entry_count as u64) + 1);

        // write metadata
        let metadata = PakMetadata::new(use_full_package_mode);
        metadata.write_to_pak(&mut pak_writer)?;

        let pak_writer_mtx = Arc::new(Mutex::new(pak_writer));

        let bar = ProgressBar::new(entry_count as u64);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("Bytes written: {msg}\n{pos}/{len} {wide_bar}")?,
        );
        bar.enable_steady_tick(Duration::from_millis(200));

        let bytes_written = Arc::new(AtomicUsize::new(0));
        let bar_evt = bar.clone();
        let bytes_written_evt = Arc::clone(&bytes_written);

        // Use extractor_callback to drive entry reads (handles chunk-table offsets internally).
        let mut extractor = pak.extractor_callback().on_event(move |event| {
            if let ExtractEvent::FileDone { .. } = event {
                bar_evt.inc(1);
                if bar_evt.position().is_multiple_of(100) {
                    bar_evt.set_message(
                        HumanBytes(bytes_written_evt.load(Ordering::SeqCst) as u64).to_string(),
                    );
                }
            }
        });

        let pak_writer_mtx1 = Arc::clone(&pak_writer_mtx);
        let bytes_written1 = Arc::clone(&bytes_written);
        let use_full_package_mode1 = use_full_package_mode;
        if let Some(tex_hashes) = tex_hashes {
            extractor = extractor.filter(move |entry, _path| tex_hashes.contains(&entry.hash()));
        }

        let err = extractor.run_with_entry_reader(move |entry, _rel_path, entry_reader| {
            let mut file_options = FileOptions::default();
            if use_feature_clone {
                let unknown_attr = entry.all_attr() & !KnownAttr::KNOWN_MASK;
                file_options = file_options.with_all_attr(unknown_attr);
            }

            if use_full_package_mode1 {
                let prefix = read_prefix(entry_reader, 8)?;
                let is_tex = determine_extension_from_bytes(&prefix) == Some("tex");
                if is_tex {
                    let mut chained = std::io::Cursor::new(prefix).chain(entry_reader);
                    let mut tex = Tex::from_reader(&mut chained)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    tex.batch_decompress()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

                    let tex_bytes = tex
                        .as_bytes()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

                    let mut pak_writer = pak_writer_mtx1.lock();
                    pak_writer
                        .start_file(entry.hash(), file_options)
                        .map_err(std::io::Error::other)?;
                    pak_writer.write_all(&tex_bytes)?;
                    bytes_written1.fetch_add(tex_bytes.len(), Ordering::SeqCst);
                } else {
                    let mut pak_writer = pak_writer_mtx1.lock();
                    pak_writer
                        .start_file(entry.hash(), file_options)
                        .map_err(std::io::Error::other)?;
                    pak_writer.write_all(&prefix)?;
                    let copied = std::io::copy(entry_reader, &mut *pak_writer)? as usize;
                    bytes_written1.fetch_add(prefix.len() + copied, Ordering::SeqCst);
                }
            } else {
                // Patch mode: only TEX entries are selected and included in output.
                let mut tex = Tex::from_reader(entry_reader)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                tex.batch_decompress()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

                let tex_bytes = tex
                    .as_bytes()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

                let mut pak_writer = pak_writer_mtx1.lock();
                pak_writer
                    .start_file(entry.hash(), file_options)
                    .map_err(std::io::Error::other)?;
                pak_writer.write_all(&tex_bytes)?;
                bytes_written1.fetch_add(tex_bytes.len(), Ordering::SeqCst);
            }

            Ok(())
        });

        if let Err(e) = err {
            eprintln!("Error occurred when processing tex: {e}");
            eprintln!(
                "The process terminated early, we'll save the current processed tex files to pak file."
            );
        }

        match Arc::try_unwrap(pak_writer_mtx) {
            Ok(pak_writer) => pak_writer.into_inner().finish()?,
            Err(_) => panic!("Arc::try_unwrap failed"),
        };

        bar.finish();

        Ok(())
    }

    fn auto_mode(&mut self) -> color_eyre::Result<()> {
        let current_dir = std::env::current_dir()?;

        wait_for_enter(
            r#"Check list:

1. Your game is already updated to the latest version.
2. Uninstalled all the mods, or the generated files will break mods.

I'm sure I've checked the list, press Enter to continue"#,
        );

        let game_dir: String = Input::<String>::with_theme(&ColorfulTheme::default())
            .show_default(true)
            .default(current_dir.to_string_lossy().to_string())
            .with_prompt("Input MonsterHunterWilds directory path")
            .interact_text()
            .unwrap()
            .trim_matches(|c| c == '\"' || c == '\'')
            .to_string();

        let game_dir = Path::new(&game_dir);
        if !game_dir.is_dir() {
            bail!("game directory not exists.");
        }

        // scan for pak files in main game directory and DLC directory
        let all_chunk_selections = self.scan_all_pak_files(game_dir)?;

        // show chunks for selection
        // only show sub chunks
        let chunk_selections: Vec<&ChunkSelection> = all_chunk_selections
            .iter()
            .filter(|chunk_selection| chunk_selection.chunk_name.sub_id().is_some())
            .collect();
        if chunk_selections.is_empty() {
            bail!("No available pak files found.");
        }

        let selected_chunks: Vec<bool> = chunk_selections
            .iter()
            .map(|chunk_selection| {
                chunk_selection.file_size >= AUTO_CHUNK_SELECTION_SIZE_THRESHOLD as u64
            })
            .collect();

        let selected_chunks: Option<Vec<usize>> =
            MultiSelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Select chunks to process (Space to select, Enter to confirm)")
                .items(&chunk_selections)
                .defaults(&selected_chunks)
                .interact_opt()?;
        let Some(selected_chunks) = selected_chunks else {
            bail!("No chunks selected.");
        };

        let selected_chunk_selections: Vec<&ChunkSelection> = selected_chunks
            .iter()
            .map(|i| chunk_selections[*i])
            .collect();

        // replace mode: replace original files with uncompressed files
        // patch mode: generate patch files after original patch files
        let use_replace_mode = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "Replace original files with uncompressed files? (Will automatically backup original files)",
            )
            .default(0)
            .items(FALSE_TRUE_SELECTION)
            .interact()
            .unwrap();
        let use_replace_mode = use_replace_mode == 1;

        // all chunk names for patch ID tracking
        let mut all_chunk_names: Vec<ChunkName> = all_chunk_selections
            .iter()
            .map(|cs| cs.chunk_name.clone())
            .collect();

        // start processing
        for chunk_selection in selected_chunk_selections {
            let chunk_path = &chunk_selection.full_path;
            let chunk_name = &chunk_selection.chunk_name;

            let output_path = if use_replace_mode {
                // In replace mode, first generate a temporary decompressed file
                chunk_path.with_extension("pak.temp")
            } else {
                // In patch mode
                // Find the max patch id for the current chunk series
                let max_patch_id = all_chunk_names
                    .iter()
                    .filter(|c| {
                        c.major_id() == chunk_name.major_id()
                            && c.patch_id() == chunk_name.patch_id()
                            && c.sub_id() == chunk_name.sub_id()
                    })
                    .filter_map(|c| c.sub_patch_id())
                    .max()
                    .unwrap_or(0);

                let new_patch_id = max_patch_id + 1;

                // Create a new chunk name
                let output_chunk_name = chunk_name.set_sub_patch(new_patch_id);

                // Add the new patch to the chunk list so it can be found in subsequent processing
                all_chunk_names.push(output_chunk_name.clone());

                // Determine output directory based on original chunk location
                let output_dir = chunk_path.parent().unwrap();
                output_dir.join(output_chunk_name.to_string())
            };

            println!("Output patch file: {}", output_path.display());
            self.process_chunk(chunk_path, &output_path, use_replace_mode, true)?;

            // In replace mode, backup the original file
            // and rename the temporary file to the original file name
            if use_replace_mode {
                // Backup the original file
                let backup_path = chunk_path.with_extension("pak.backup");
                if backup_path.exists() {
                    fs::remove_file(&backup_path)?;
                }
                fs::rename(chunk_path, &backup_path)?;
                // Rename the temporary file to the original file name
                fs::rename(&output_path, chunk_path)?;
            }
            println!();
        }

        Ok(())
    }

    fn manual_mode(&mut self) -> color_eyre::Result<()> {
        let input: String = Input::with_theme(&ColorfulTheme::default())
            .show_default(true)
            .default("re_chunk_000.pak.sub_000.pak".to_string())
            .with_prompt("Input .pak file path")
            .interact_text()
            .unwrap()
            .trim_matches(|c| c == '\"' || c == '\'')
            .to_string();

        let input_path = Path::new(&input);
        if !input_path.is_file() {
            bail!("input file not exists.");
        }

        let use_full_package_mode = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "Package all files, including non-tex files (for replacing original files)",
            )
            .default(0)
            .items(FALSE_TRUE_SELECTION)
            .interact()
            .unwrap();
        let use_full_package_mode = use_full_package_mode == 1;

        let use_feature_clone = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Clone feature flags from original file?")
            .default(1)
            .items(FALSE_TRUE_SELECTION)
            .interact()
            .unwrap();
        let use_feature_clone = use_feature_clone == 1;

        self.process_chunk(
            input_path,
            &input_path.with_extension("uncompressed.pak"),
            use_full_package_mode,
            use_feature_clone,
        )?;

        Ok(())
    }

    fn restore_mode(&mut self) -> color_eyre::Result<()> {
        let current_dir = std::env::current_dir()?;

        let game_dir: String = Input::<String>::with_theme(&ColorfulTheme::default())
            .show_default(true)
            .default(current_dir.to_string_lossy().to_string())
            .with_prompt("Input MonsterHunterWilds directory path")
            .interact_text()
            .unwrap()
            .trim_matches(|c| c == '\"' || c == '\'')
            .to_string();

        let game_dir = Path::new(&game_dir);
        if !game_dir.is_dir() {
            bail!("game directory not exists.");
        }

        // scan all pak files, find files generated by this tool
        println!("Scanning tool generated files...");
        let mut tool_generated_files = Vec::new();
        let mut backup_files = Vec::new();
        let mut all_chunks = Vec::new();

        // Scan main directory
        self.scan_tool_files_in_directory(
            game_dir,
            &mut tool_generated_files,
            &mut backup_files,
            &mut all_chunks,
        )?;

        // Scan DLC directory if exists
        let dlc_dir = game_dir.join("dlc");
        if dlc_dir.is_dir() {
            self.scan_tool_files_in_directory(
                &dlc_dir,
                &mut tool_generated_files,
                &mut backup_files,
                &mut all_chunks,
            )?;
        }

        if tool_generated_files.is_empty() && backup_files.is_empty() {
            println!("No files found to restore.");
            return Ok(());
        }

        println!(
            "Found {} tool generated files and {} backup files",
            tool_generated_files.len(),
            backup_files.len()
        );

        // restore
        let mut patch_files_to_remove = Vec::new();
        for (file_path, metadata) in &tool_generated_files {
            if metadata.is_full_package() {
                // restore full package mode (replace mode)
                // this is a replace mode generated file, find the corresponding backup file
                let backup_path = file_path.with_extension("pak.backup");
                if backup_path.exists() {
                    println!("Restore replace mode file: {}", file_path.display());

                    // delete the current file and restore the backup
                    fs::remove_file(file_path)?;
                    fs::rename(&backup_path, file_path)?;

                    println!("   Restore backup file: {}", backup_path.display());
                } else {
                    println!("Warning: backup file not found {}", backup_path.display());
                }
            } else {
                // restore patch mode
                // this is a patch mode generated file
                if let Ok(chunk_name) =
                    ChunkName::try_from_str(&file_path.file_name().unwrap().to_string_lossy())
                {
                    patch_files_to_remove.push((file_path.clone(), chunk_name));
                }
            }
        }

        // remove patch files
        if !patch_files_to_remove.is_empty() {
            println!("Remove patch files...");

            for (file_path, chunk_name) in patch_files_to_remove.iter().rev() {
                println!("Remove patch file: {}", file_path.display());

                // Check if there are any patches with higher numbers
                let has_higher_patches = all_chunks.iter().any(|c| {
                    c.major_id() == chunk_name.major_id()
                        && c.sub_id() == chunk_name.sub_id()
                        && match (c.sub_id(), c.sub_patch_id()) {
                            (Some(_), Some(patch_id)) => {
                                patch_id > chunk_name.sub_patch_id().unwrap()
                            }
                            (None, Some(patch_id)) => patch_id > chunk_name.patch_id().unwrap(),
                            _ => false,
                        }
                });

                if has_higher_patches {
                    // create an empty patch file instead of deleting, to keep the patch sequence continuous
                    self.create_empty_patch_file(file_path)?;
                    println!("   Create empty patch file to keep sequence continuous");
                } else {
                    // no higher patches exist, safe to delete
                    fs::remove_file(file_path)?;
                    // remove from all_chunks
                    all_chunks.retain(|c| c != chunk_name);
                    println!("   Removed patch file");
                }
            }
        }

        println!("Restore completed!");
        Ok(())
    }

    /// Scan tool generated files in a specific directory
    fn scan_tool_files_in_directory(
        &self,
        dir: &Path,
        tool_generated_files: &mut Vec<(std::path::PathBuf, PakMetadata)>,
        backup_files: &mut Vec<std::path::PathBuf>,
        all_chunks: &mut Vec<ChunkName>,
    ) -> color_eyre::Result<()> {
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();
            let file_path = entry.path();

            // check backup files
            if file_name.ends_with(".pak.backup") {
                backup_files.push(file_path);
                continue;
            }

            // check pak files
            if !file_name.ends_with(".pak") {
                continue;
            }

            // Check if it's a chunk or DLC file
            let is_chunk = file_name.starts_with("re_chunk_");
            let is_dlc = file_name.starts_with("re_dlc_");

            if !is_chunk && !is_dlc {
                continue;
            }

            // collect chunk info
            if let Ok(chunk_name) = ChunkName::try_from_str(&file_name) {
                all_chunks.push(chunk_name.clone());
            }

            // check if the file is generated by this tool
            if let Ok(Some(metadata)) = self.check_tool_generated_file(&file_path) {
                tool_generated_files.push((file_path, metadata));
            }
        }
        Ok(())
    }

    /// check if the file is generated by this tool, return metadata
    fn check_tool_generated_file(
        &self,
        file_path: &Path,
    ) -> color_eyre::Result<Option<PakMetadata>> {
        let file = match fs::File::open(file_path) {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };

        let mut reader = io::BufReader::new(file);
        let pak_metadata = match ree_pak_core::read::read_metadata(&mut reader) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(None),
        };

        PakMetadata::from_pak_metadata(reader, &pak_metadata)
    }

    /// create an empty patch file
    fn create_empty_patch_file(&self, file_path: &Path) -> color_eyre::Result<()> {
        let out_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(file_path)?;

        let mut pak_writer = ree_pak_core::write::PakWriter::new(out_file, 1);

        // write metadata to mark this is an empty patch file
        let metadata = PakMetadata::new(false);
        metadata.write_to_pak(&mut pak_writer)?;

        pak_writer.finish()?;
        Ok(())
    }
}

fn read_prefix<R>(reader: &mut R, max_len: usize) -> io::Result<Vec<u8>>
where
    R: Read,
{
    let mut buf = vec![0u8; max_len];
    let mut read_len = 0usize;
    while read_len < max_len {
        let n = reader.read(&mut buf[read_len..])?;
        if n == 0 {
            break;
        }
        read_len += n;
    }
    buf.truncate(read_len);
    Ok(buf)
}

fn wait_for_enter(msg: &str) {
    let _: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt(msg)
        .allow_empty(true)
        .interact_text()
        .unwrap();
}
