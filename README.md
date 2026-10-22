# Image Browser

A Linux desktop file browser built with Rust, focused on quickly browsing and organizing image collections.

## Features

- Browse image files in a folder and open a larger preview by double-clicking a thumbnail.
- Load image pixels and thumbnails lazily as cards enter the visible viewport.
- Show live progress while folders are scanned and images are grouped.
- Increase or decrease thumbnail size.
- Group images by dimensions, approximate file size (within 10%), or file type.
- Select multiple images and copy or move them to another folder.
- Delete selected images after confirming the permanent deletion.
- View selected-file names, types, dimensions, sizes, and timestamps in a dedicated metadata panel.
- Existing files are never overwritten: copies and moves receive a numbered name when needed.

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
