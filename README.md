# Image Browser

A Linux desktop file browser built with Rust, focused on quickly browsing and organizing image collections.

## Features

- Browse image files in a folder and open a larger preview by double-clicking a thumbnail.
- Decode thumbnails in the background, in the current display order, while keeping the folder view responsive.
- Show live progress while folders are scanned and images are grouped.
- Increase or decrease thumbnail size.
- Group images by dimensions, approximate file size (within 10%), or file type.
- Select multiple images and copy or move them to another folder.
- Delete selected images after confirming the permanent deletion.
- Right-click a thumbnail to copy, move, or delete images, or set a single selected image as the desktop wallpaper.
- Detect connected displays; GNOME and Cinnamon set the wallpaper globally, while KDE Plasma supports individual, selected multiple, or all displays.
- Set a selected-image slideshow across all displays with a persistent user service; its interval slider ranges from 5 seconds to 1 year and image changes follow UTC system-clock boundaries. Right-click and choose “Stop wallpaper slideshow” to stop it.
- View selected-file names, types, dimensions, sizes, and modification times in a dedicated metadata panel.
- Existing files are never overwritten: copies and moves receive a numbered name when needed.

## Supported formats

The built-in Rust image decoders support AVIF, BMP, DDS, EXR, Farbfeld, GIF, HDR, ICO, JPEG, PNG, PNM (including PBM, PGM, PPM, PAM, and PFM), QOI, TGA, TIFF, and WebP. Animated formats display their first frame. Format support is limited to codecs available in the open-source `image` crate; proprietary RAW camera formats are not included.

## Run

Install Rust, `pkg-config`, and the dav1d development package used to decode AVIF (`libdav1d-dev` on Ubuntu and Linux Mint), then run:

```sh
sudo apt install pkg-config libdav1d-dev
cargo run --release
```

Persistent wallpaper slideshows require a systemd user session and Python 3; the desktop's wallpaper tools (GSettings on GNOME/Cinnamon or `qdbus` on KDE Plasma) must also be available.

Build a Linux executable with:

```sh
cargo build --release
```

## Linux install packages

The **Linux packages** GitHub Actions workflow builds installable `.deb` packages for Debian and Ubuntu, `.rpm` packages for Fedora and openSUSE, and `.pkg.tar.zst` packages for Arch Linux. Push a version tag such as `v0.1.0` to attach all three packages to its GitHub Release. Builds on `main` and manually started workflow runs publish the packages as downloadable GitHub Actions artifacts.

The packages install the application binary and desktop launcher. Runtime libraries such as GTK 3, Wayland, and OpenGL are provided by the distribution package repositories; the workflow statically builds dav1d to avoid distro-specific runtime versions.
