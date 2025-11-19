# MHWs Tex Decompressor

A tool to make a pak with uncompressed textures for MHWilds.

## Usage

### Windows

1. Download from [Releases](https://github.com/eigeen/mhws-tex-decompressor/releases).
2. If it is a zip file, extract it.
3. Run exe file, follow the instructions.

### Linux & MacOS

1. Install Rust: https://rustup.rs/

```sh
# 2. Clone the repository
git clone https://github.com/eigeen/mhws-tex-decompressor

# 3. Build and run the program
cd mhws-tex-decompressor
cargo run --release
```

Or using Nix if you prefer:

``` sh
nix run github:eigeen/mhws-tex-decompressor
```

## Credits

[@AsteriskAmpersand](https://github.com/AsteriskAmpersand) for the original texture conversion code and the idea of using uncompressed textures.
