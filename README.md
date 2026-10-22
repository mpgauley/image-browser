# Image Browser

A Linux desktop file browser built with Rust, focused on quickly browsing and organizing image collections.

## Features

- Browse image files in a folder and open a larger preview by double-clicking a thumbnail.
- Increase or decrease thumbnail size.
- Group images by dimensions, approximate file size (within 10%), visual similarity, or file type.
- Select multiple images and copy or move them to another folder.
- Existing files are never overwritten: copies and moves receive a numbered name when needed.

Visual duplicate groups use a difference hash and are intended to find possible lookalikes, not prove that files are identical. Hashing is performed when that grouping is selected.

## Supported formats

The built-in Rust image decoders support AVIF, BMP, DDS, EXR, Farbfeld, GIF, HDR, ICO, JPEG, PNG, PNM (including PBM, PGM, PPM, PAM, and PFM), QOI, TGA, TIFF, and WebP. Animated formats display their first frame. Format support is limited to codecs available in the open-source `image` crate; proprietary RAW camera formats are not included.

## Run

Install Rust, `pkg-config`, and the dav1d development package used to decode AVIF (`libdav1d-dev` on Ubuntu and Linux Mint), then run:

```sh
sudo apt install pkg-config libdav1d-dev
cargo run --release
```

Build a Linux executable with:

```sh
cargo build --release
```
