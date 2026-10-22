# Changelog

## Unreleased — 2026-10-22

### Added

- Added a GitHub Actions workflow that builds Debian/Ubuntu `.deb`, Fedora/openSUSE `.rpm`, and Arch Linux packages, uploads them as workflow artifacts, and attaches them to GitHub Releases for version tags.
- Added thumbnail context-menu actions to copy, move, or delete selected images.
- Added wallpaper actions for GNOME, Cinnamon, and KDE Plasma, including display detection and KDE per-display, selected-display, and all-display options.
- Added persistent wallpaper slideshows for multiple selected images. The interval slider ranges from 5 seconds to 1 year, and image changes follow UTC system-clock boundaries. A systemd user service keeps the slideshow running after Image Browser closes; the context menu can stop it.
- Added background thumbnail decoding in display order to keep browsing responsive.

### Changed

- Removed the creation date from selected-file metadata; the modification date remains.
- Updated the README with wallpaper, slideshow, and runtime requirements.
