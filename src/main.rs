use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use image::imageops::FilterType;

const THUMBNAIL_MAX_EDGE: u32 = 512;
const PREVIEW_MAX_EDGE: u32 = 2048;
const DUPLICATE_HASH_DISTANCE: u32 = 6;
const SIMILAR_SIZE_TOLERANCE: f64 = 0.10;
const GRID_SPACING: f32 = 8.0;
const GRID_RIGHT_PADDING: f32 = 10.0;
const CARD_FRAME_HORIZONTAL_MARGIN: f32 = 28.0;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Image Browser",
        options,
        Box::new(|cc| Ok(Box::new(ImageBrowserApp::new(cc)))),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GroupMode {
    #[default]
    None,
    Dimensions,
    SimilarSize,
    Duplicates,
    FileType,
}

impl GroupMode {
    fn label(self) -> &'static str {
        match self {
            Self::None => "All images",
            Self::Dimensions => "Dimensions",
            Self::SimilarSize => "Similar file size",
            Self::Duplicates => "Possible duplicates",
            Self::FileType => "File type",
        }
    }
}

struct ImageEntry {
    path: PathBuf,
    dimensions: Option<(u32, u32)>,
    file_size: u64,
    extension: String,
    thumbnail: Option<TextureHandle>,
    perceptual_hash: Option<Option<u64>>,
}

struct Preview {
    path: PathBuf,
    texture: Option<TextureHandle>,
    error: Option<String>,
}

struct ImageBrowserApp {
    current_dir: Option<PathBuf>,
    path_input: String,
    images: Vec<ImageEntry>,
    selected: HashSet<PathBuf>,
    selection_anchor: Option<PathBuf>,
    confirm_delete: bool,
    thumbnail_size: f32,
    group_mode: GroupMode,
    preview: Option<Preview>,
    status: String,
}

impl ImageBrowserApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let current_dir = default_directory();
        let mut app = Self {
            path_input: current_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            current_dir,
            images: Vec::new(),
            selected: HashSet::new(),
            selection_anchor: None,
            confirm_delete: false,
            thumbnail_size: 160.0,
            group_mode: GroupMode::None,
            preview: None,
            status: String::new(),
        };

        if app.current_dir.is_some() {
            app.refresh();
        } else {
            app.status = "Could not determine the current user's home directory.".to_owned();
        }
        app
    }

    fn refresh(&mut self) {
        let Some(directory) = self.current_dir.as_deref() else {
            self.status = "Choose a folder to browse.".to_owned();
            return;
        };

        match scan_directory(directory) {
            Ok((images, skipped)) => {
                let count = images.len();
                self.images = images;
                self.selected.clear();
                self.selection_anchor = None;
                self.status = if skipped > 0 {
                    format!("Loaded {count} images; skipped {skipped} unreadable file(s).")
                } else {
                    format!("Loaded {count} images.")
                };
            }
            Err(error) => self.status = format!("Could not open folder: {error:#}"),
        }
    }

    fn set_directory(&mut self, directory: PathBuf) {
        self.path_input = directory.display().to_string();
        self.current_dir = Some(directory);
        self.refresh();
    }

    fn set_group_mode(&mut self, mode: GroupMode) {
        self.group_mode = mode;
        if mode == GroupMode::Duplicates {
            let mut failed = 0;
            for entry in &mut self.images {
                if entry.perceptual_hash.is_none() {
                    entry.perceptual_hash = match perceptual_hash(&entry.path) {
                        Ok(hash) => Some(Some(hash)),
                        Err(_) => {
                            failed += 1;
                            Some(None)
                        }
                    };
                }
            }
            self.status = if failed == 0 {
                "Possible duplicates are grouped by visual similarity.".to_owned()
            } else {
                format!("Could not analyze {failed} image(s) for visual similarity.")
            };
        }
    }

    fn copy_or_move_selected(&mut self, moving: bool) {
        if self.selected.is_empty() {
            self.status = "Select one or more images first.".to_owned();
            return;
        }

        let Some(current_dir) = self.current_dir.as_deref() else {
            self.status = "Choose a folder to browse first.".to_owned();
            return;
        };
        let Some(destination) = rfd::FileDialog::new()
            .set_directory(current_dir)
            .pick_folder()
        else {
            return;
        };

        let mut moved_paths = HashSet::new();
        let mut completed = 0;
        let mut failures = Vec::new();
        let selected: Vec<_> = self.selected.iter().cloned().collect();
        for source in selected {
            let result = if moving {
                move_to_directory(&source, &destination)
            } else {
                copy_to_directory(&source, &destination)
            };
            match result {
                Ok(()) => {
                    completed += 1;
                    if moving {
                        moved_paths.insert(source);
                    }
                }
                Err(error) => failures.push(format!(
                    "{}: {error:#}",
                    source.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }

        if moving {
            self.images
                .retain(|entry| !moved_paths.contains(&entry.path));
            self.selected.retain(|path| !moved_paths.contains(path));
        }
        self.status = match (completed, failures.is_empty()) {
            (count, true) => format!(
                "{} {count} image(s).",
                if moving { "Moved" } else { "Copied" }
            ),
            (0, false) => format!("No images were transferred. {}", failures.join(" | ")),
            (count, false) => format!(
                "{} {count} image(s); {} failed: {}",
                if moving { "Moved" } else { "Copied" },
                failures.len(),
                failures.join(" | ")
            ),
        };
    }

    fn delete_selected(&mut self) {
        if self.selected.is_empty() {
            self.status = "Select one or more images first.".to_owned();
            return;
        }

        let selected: Vec<_> = self.selected.iter().cloned().collect();
        let (deleted, failures) = delete_files(&selected);
        self.images.retain(|entry| !deleted.contains(&entry.path));
        self.selected.retain(|path| !deleted.contains(path));
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|path| deleted.contains(path))
        {
            self.selection_anchor = None;
        }
        self.status = match (deleted.len(), failures.is_empty()) {
            (count, true) => format!("Deleted {count} image(s)."),
            (0, false) => format!("No images were deleted. {}", failures.join(" | ")),
            (count, false) => format!(
                "Deleted {count} image(s); {} failed: {}",
                failures.len(),
                failures.join(" | ")
            ),
        };
    }

    fn image_groups(&self) -> Vec<(String, Vec<usize>)> {
        match self.group_mode {
            GroupMode::None => vec![("Images".to_owned(), (0..self.images.len()).collect())],
            GroupMode::Dimensions => group_by_key(&self.images, |entry| match entry.dimensions {
                Some((width, height)) => format!("{width} × {height}"),
                None => "Unknown dimensions".to_owned(),
            }),
            GroupMode::FileType => group_by_key(&self.images, |entry| {
                if entry.extension.is_empty() {
                    "Unknown file type".to_owned()
                } else {
                    format!(".{}", entry.extension.to_uppercase())
                }
            }),
            GroupMode::SimilarSize => similar_size_groups(&self.images),
            GroupMode::Duplicates => duplicate_groups(&self.images),
        }
    }

    fn draw_top_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Folder:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .desired_width(350.0)
                        .hint_text("Enter a folder path"),
                );
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.open_path_input();
                }
                if ui.button("Open").clicked() {
                    self.open_path_input();
                }
                if ui.button("Browse…").clicked()
                    && let Some(directory) = rfd::FileDialog::new().pick_folder()
                {
                    self.set_directory(directory);
                }
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
            });

            ui.horizontal(|ui| {
                ui.label("Group:");
                for mode in [
                    GroupMode::None,
                    GroupMode::Dimensions,
                    GroupMode::SimilarSize,
                    GroupMode::Duplicates,
                    GroupMode::FileType,
                ] {
                    if ui
                        .selectable_label(self.group_mode == mode, mode.label())
                        .clicked()
                    {
                        self.set_group_mode(mode);
                    }
                }
            });

            ui.horizontal(|ui| {
                if ui
                    .button("−")
                    .on_hover_text("Decrease thumbnail size")
                    .clicked()
                {
                    self.thumbnail_size = (self.thumbnail_size - 24.0).max(80.0);
                }
                ui.add(
                    egui::Slider::new(&mut self.thumbnail_size, 80.0..=320.0)
                        .show_value(false)
                        .text("Thumbnail size"),
                );
                if ui
                    .button("+")
                    .on_hover_text("Increase thumbnail size")
                    .clicked()
                {
                    self.thumbnail_size = (self.thumbnail_size + 24.0).min(320.0);
                }
                ui.separator();
                ui.label(format!("{} selected", self.selected.len()));
                if ui.button("Copy to…").clicked() {
                    self.copy_or_move_selected(false);
                }
                if ui.button("Move to…").clicked() {
                    self.copy_or_move_selected(true);
                }
                if ui
                    .add_enabled(!self.selected.is_empty(), egui::Button::new("Delete…"))
                    .clicked()
                {
                    self.confirm_delete = true;
                }
            });
        });
    }

    fn open_path_input(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        if path.is_dir() {
            self.set_directory(path);
        } else {
            self.status = format!("Not a folder: {}", path.display());
        }
    }

    fn draw_image_grid(&mut self, ctx: &egui::Context) {
        let groups = self.image_groups();
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.images.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("No supported images in this folder.");
                    });
                    return;
                }

                let mut preview_path = None;
                let available_width = ui.available_width();
                let thumbnail_size = fit_thumbnail_size(available_width, self.thumbnail_size);
                let column_count = grid_column_count(available_width, thumbnail_size);
                let display_order: Vec<_> = groups
                    .iter()
                    .flat_map(|(_, indices)| {
                        indices.iter().map(|&index| self.images[index].path.clone())
                    })
                    .collect();
                for (group_name, image_indices) in groups {
                    ui.heading(format!("{group_name} ({})", image_indices.len()));
                    ui.add_space(4.0);
                    egui::Grid::new((
                        "image-grid",
                        &group_name,
                        column_count,
                        thumbnail_size.to_bits(),
                    ))
                    .num_columns(column_count)
                    .spacing(egui::vec2(GRID_SPACING, GRID_SPACING))
                    .show(ui, |ui| {
                        for (position, index) in image_indices.into_iter().enumerate() {
                            let entry = &mut self.images[index];
                            let selected = self.selected.contains(&entry.path);
                            let (clicked, double_clicked) =
                                draw_image_card(ui, ctx, entry, selected, thumbnail_size);
                            if clicked {
                                let (shift, toggle) = ctx.input(|input| {
                                    (
                                        input.modifiers.shift,
                                        input.modifiers.ctrl || input.modifiers.command,
                                    )
                                });
                                select_image(
                                    &mut self.selected,
                                    &mut self.selection_anchor,
                                    &display_order,
                                    &entry.path,
                                    shift,
                                    toggle,
                                );
                            }
                            if double_clicked {
                                preview_path = Some(entry.path.clone());
                            }
                            if (position + 1) % column_count == 0 {
                                ui.end_row();
                            }
                        }
                    });
                    ui.add_space(12.0);
                }
                if let Some(path) = preview_path {
                    self.preview = Some(Preview {
                        path,
                        texture: None,
                        error: None,
                    });
                }
            });
        });
    }

    fn draw_preview(&mut self, ctx: &egui::Context) {
        let Some(preview) = self.preview.as_mut() else {
            return;
        };

        if preview.texture.is_none() && preview.error.is_none() {
            match load_texture(ctx, &preview.path, PREVIEW_MAX_EDGE) {
                Ok(texture) => preview.texture = Some(texture),
                Err(error) => preview.error = Some(format!("{error:#}")),
            }
        }

        let title = preview
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .default_size([720.0, 600.0])
            .resizable(true)
            .show(ctx, |ui| {
                if let Some(error) = &preview.error {
                    ui.label(format!("Could not display image: {error}"));
                } else if let Some(texture) = &preview.texture {
                    let available = ui.available_size();
                    ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(available)
                            .maintain_aspect_ratio(true),
                    );
                }
            });
        if !open {
            self.preview = None;
        }
    }

    fn draw_delete_confirmation(&mut self, ctx: &egui::Context) {
        if !self.confirm_delete {
            return;
        }

        let mut confirm = false;
        let mut cancel = false;
        let response = egui::Modal::new(egui::Id::new("confirm-delete")).show(ctx, |ui| {
            ui.heading("Delete selected images?");
            ui.label(format!(
                "This permanently deletes {} image(s).",
                self.selected.len()
            ));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if ui.button("Delete").clicked() {
                    confirm = true;
                }
            });
        });

        if confirm {
            self.confirm_delete = false;
            self.delete_selected();
        } else if cancel || response.should_close() {
            self.confirm_delete = false;
        }
    }
}

impl eframe::App for ImageBrowserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw_top_panel(ctx);
        self.draw_image_grid(ctx);
        self.draw_preview(ctx);
        self.draw_delete_confirmation(ctx);

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if !self.images.is_empty() {
                    ui.separator();
                    ui.label(format!("{} images", self.images.len()));
                }
            });
        });
    }
}

fn fit_thumbnail_size(available_width: f32, requested_size: f32) -> f32 {
    (available_width - GRID_RIGHT_PADDING - CARD_FRAME_HORIZONTAL_MARGIN)
        .max(1.0)
        .min(requested_size)
}

fn grid_column_count(available_width: f32, thumbnail_size: f32) -> usize {
    let card_width = thumbnail_size + CARD_FRAME_HORIZONTAL_MARGIN;
    let mut columns = ((available_width - GRID_RIGHT_PADDING + GRID_SPACING)
        / (card_width + GRID_SPACING))
        .floor()
        .max(1.0) as usize;
    while columns > 1
        && grid_cards_width(columns, card_width) > available_width - GRID_RIGHT_PADDING
    {
        columns -= 1;
    }
    columns
}

fn grid_cards_width(columns: usize, card_width: f32) -> f32 {
    columns as f32 * card_width + columns.saturating_sub(1) as f32 * GRID_SPACING
}

fn select_image(
    selected: &mut HashSet<PathBuf>,
    anchor: &mut Option<PathBuf>,
    display_order: &[PathBuf],
    clicked_path: &Path,
    shift: bool,
    toggle: bool,
) {
    if shift {
        let clicked_index = display_order.iter().position(|path| path == clicked_path);
        let anchor_index = anchor
            .as_deref()
            .and_then(|anchor_path| display_order.iter().position(|path| path == anchor_path));
        if let Some(clicked_index) = clicked_index {
            let anchor_index = anchor_index.unwrap_or(clicked_index);
            if !toggle {
                selected.clear();
            }
            for path in
                &display_order[anchor_index.min(clicked_index)..=anchor_index.max(clicked_index)]
            {
                selected.insert(path.clone());
            }
            if anchor.is_none() {
                *anchor = Some(clicked_path.to_path_buf());
            }
        }
    } else if toggle {
        if !selected.remove(clicked_path) {
            selected.insert(clicked_path.to_path_buf());
        }
        *anchor = Some(clicked_path.to_path_buf());
    } else {
        selected.clear();
        selected.insert(clicked_path.to_path_buf());
        *anchor = Some(clicked_path.to_path_buf());
    }
}

fn supported_extension(extension: &str) -> bool {
    matches!(
        extension,
        "avif"
            | "bmp"
            | "dds"
            | "exr"
            | "ff"
            | "gif"
            | "hdr"
            | "ico"
            | "jpeg"
            | "jpg"
            | "pam"
            | "pbm"
            | "pfm"
            | "pgm"
            | "png"
            | "pnm"
            | "ppm"
            | "qoi"
            | "tga"
            | "tif"
            | "tiff"
            | "webp"
    )
}

fn default_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            std::env::var_os("USER")
                .map(|username| PathBuf::from("/home").join(username))
                .filter(|path| path.is_dir())
        })
}

fn scan_directory(directory: &Path) -> Result<(Vec<ImageEntry>, usize)> {
    let entries =
        fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?;
    let mut images = Vec::new();
    let mut skipped = 0;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !supported_extension(&extension) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let dimensions = image::image_dimensions(&path).ok();
        images.push(ImageEntry {
            path,
            dimensions,
            file_size: metadata.len(),
            extension,
            thumbnail: None,
            perceptual_hash: None,
        });
    }

    images.sort_by_cached_key(|entry| {
        entry
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    });
    Ok((images, skipped))
}

fn draw_image_card(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    entry: &mut ImageEntry,
    selected: bool,
    thumbnail_size: f32,
) -> (bool, bool) {
    if entry.thumbnail.is_none()
        && let Ok(texture) = load_texture(ctx, &entry.path, THUMBNAIL_MAX_EDGE)
    {
        entry.thumbnail = Some(texture);
    }

    let mut clicked = false;
    let mut double_clicked = false;
    egui::Frame::group(ui.style())
        .fill(if selected {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().faint_bg_color
        })
        .show(ui, |ui| {
            ui.set_width(thumbnail_size + 16.0);
            let thumbnail_area = egui::vec2(thumbnail_size, thumbnail_size);
            if let Some(texture) = &entry.thumbnail {
                let texture_size = texture.size_vec2();
                let factor = (thumbnail_size / texture_size.x).min(thumbnail_size / texture_size.y);
                let size = texture_size * factor;
                ui.allocate_ui_with_layout(
                    thumbnail_area,
                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                    |ui| {
                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::click()),
                        );
                        clicked = response.clicked();
                        double_clicked = response.double_clicked();
                    },
                );
            } else {
                ui.allocate_ui_with_layout(
                    thumbnail_area,
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.label("Preview unavailable");
                    },
                );
            }
        });
    ui.horizontal(|ui| {
        let file_type = if entry.extension.is_empty() {
            "Unknown file type".to_owned()
        } else {
            format!(".{}", entry.extension.to_uppercase())
        };
        let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let name_response = ui
            .add_sized(
                [thumbnail_size - 48.0, ui.spacing().interact_size.y],
                egui::Label::new(name)
                    .truncate()
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(entry.path.display().to_string());
        clicked |= name_response.clicked();
        double_clicked |= name_response.double_clicked();
        ui.small(file_type);
    });
    match entry.dimensions {
        Some((width, height)) => ui.small(format!("{width} × {height}")),
        None => ui.small("Unknown dimensions"),
    };
    ui.small(format_file_size(entry.file_size));
    (clicked, double_clicked)
}

fn load_texture(ctx: &egui::Context, path: &Path, max_edge: u32) -> Result<TextureHandle> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    let thumbnail = image.thumbnail(max_edge, max_edge).to_rgba8();
    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
    let color_image = ColorImage::from_rgba_unmultiplied(size, thumbnail.as_raw());
    Ok(ctx.load_texture(
        path.display().to_string(),
        color_image,
        TextureOptions::LINEAR,
    ))
}

fn group_by_key(
    images: &[ImageEntry],
    key: impl Fn(&ImageEntry) -> String,
) -> Vec<(String, Vec<usize>)> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, entry) in images.iter().enumerate() {
        groups.entry(key(entry)).or_default().push(index);
    }
    groups.into_iter().collect()
}

fn delete_files(paths: &[PathBuf]) -> (HashSet<PathBuf>, Vec<String>) {
    let mut deleted = HashSet::new();
    let mut failures = Vec::new();
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {
                deleted.insert(path.clone());
            }
            Err(error) => failures.push(format!(
                "{}: {error}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )),
        }
    }
    (deleted, failures)
}

fn similar_size_groups(images: &[ImageEntry]) -> Vec<(String, Vec<usize>)> {
    let mut sorted: Vec<_> = (0..images.len()).collect();
    sorted.sort_by_key(|&index| images[index].file_size);

    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut anchor = 0;
    for index in sorted {
        let size = images[index].file_size;
        if current.is_empty() {
            anchor = size;
            current.push(index);
        } else if (size as f64) <= (anchor as f64) * (1.0 + SIMILAR_SIZE_TOLERANCE) {
            current.push(index);
        } else {
            groups.push(size_group(images, std::mem::take(&mut current)));
            anchor = size;
            current.push(index);
        }
    }
    if !current.is_empty() {
        groups.push(size_group(images, current));
    }
    groups
}

fn size_group(images: &[ImageEntry], indices: Vec<usize>) -> (String, Vec<usize>) {
    let smallest = indices
        .iter()
        .map(|&index| images[index].file_size)
        .min()
        .unwrap_or_default();
    let largest = indices
        .iter()
        .map(|&index| images[index].file_size)
        .max()
        .unwrap_or_default();
    (
        format!(
            "Similar size: {}–{}",
            format_file_size(smallest),
            format_file_size(largest)
        ),
        indices,
    )
}

fn duplicate_groups(images: &[ImageEntry]) -> Vec<(String, Vec<usize>)> {
    let mut parent: Vec<usize> = (0..images.len()).collect();
    let mut has_duplicate = vec![false; images.len()];
    for left in 0..images.len() {
        let Some(left_hash) = images[left].perceptual_hash.flatten() else {
            continue;
        };
        for right in (left + 1)..images.len() {
            let Some(right_hash) = images[right].perceptual_hash.flatten() else {
                continue;
            };
            if (left_hash ^ right_hash).count_ones() <= DUPLICATE_HASH_DISTANCE {
                let left_root = find_root(&mut parent, left);
                let right_root = find_root(&mut parent, right);
                parent[right_root] = left_root;
                has_duplicate[left] = true;
                has_duplicate[right] = true;
            }
        }
    }

    let mut grouped: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, &is_duplicate) in has_duplicate.iter().enumerate() {
        if is_duplicate {
            let root = find_root(&mut parent, index);
            grouped.entry(root).or_default().push(index);
        }
    }
    let mut groups: Vec<_> = grouped
        .into_values()
        .map(|indices| {
            let label = format!("Possible duplicates ({})", indices.len());
            (label, indices)
        })
        .collect();
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    groups
}

fn find_root(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

fn perceptual_hash(path: &Path) -> Result<u64> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    let sample = image.resize_exact(9, 8, FilterType::Triangle).to_luma8();
    let mut hash = 0_u64;
    for y in 0..8 {
        for x in 0..8 {
            if sample.get_pixel(x, y)[0] > sample.get_pixel(x + 1, y)[0] {
                hash |= 1 << (y * 8 + x);
            }
        }
    }
    Ok(hash)
}

fn copy_to_directory(source: &Path, destination: &Path) -> Result<()> {
    ensure_distinct_directories(source, destination)?;
    let mut source_file =
        File::open(source).with_context(|| format!("opening {}", source.display()))?;
    let (target, mut target_file) = create_unique_target(source, destination)?;
    let copy_result = io::copy(&mut source_file, &mut target_file)
        .with_context(|| format!("copying {} to {}", source.display(), target.display()));
    if let Err(error) = copy_result {
        return match fs::remove_file(&target) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(error.context(format!(
                "also could not remove incomplete destination {}: {cleanup_error}",
                target.display()
            ))),
        };
    }
    Ok(())
}

fn move_to_directory(source: &Path, destination: &Path) -> Result<()> {
    ensure_distinct_directories(source, destination)?;
    let mut number = 0;
    let target = loop {
        let candidate = target_candidate(source, destination, number)?;
        match fs::hard_link(source, &candidate) {
            Ok(()) => break Some(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                number = number
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("ran out of available destination names"))?;
            }
            Err(_) => break None,
        }
    };
    if target.is_some() {
        fs::remove_file(source)
            .with_context(|| format!("removing moved source {}", source.display()))?;
        return Ok(());
    }

    copy_to_directory(source, destination)?;
    fs::remove_file(source)
        .with_context(|| format!("removing source {} after copying it", source.display()))?;
    Ok(())
}

fn ensure_distinct_directories(source: &Path, destination: &Path) -> Result<()> {
    let source_directory = source
        .parent()
        .ok_or_else(|| anyhow!("source has no parent folder"))?;
    let source_directory = fs::canonicalize(source_directory)
        .with_context(|| format!("resolving {}", source_directory.display()))?;
    let destination = fs::canonicalize(destination)
        .with_context(|| format!("resolving {}", destination.display()))?;
    if source_directory == destination {
        return Err(anyhow!("source and destination folders are the same"));
    }
    Ok(())
}

fn target_candidate(source: &Path, destination: &Path, number: u64) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("source has no file name"))?;
    if number == 0 {
        return Ok(destination.join(file_name));
    }

    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let extension = source
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    Ok(destination.join(format!("{stem} (copy {number}){extension}")))
}

fn create_unique_target(source: &Path, destination: &Path) -> Result<(PathBuf, File)> {
    let mut number = 0;
    loop {
        let candidate = target_candidate(source, destination, number)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                number = number
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("ran out of available destination names"))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating destination file {}", candidate.display()));
            }
        }
    }
}

fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "image-browser-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn delete_files_removes_selected_files_and_reports_failures() {
        let root = temp_test_root("delete");
        let existing = root.join("selected.jpg");
        let missing = root.join("missing.jpg");
        fs::write(&existing, b"image").unwrap();

        let (deleted, failures) = delete_files(&[existing.clone(), missing]);

        assert_eq!(deleted, HashSet::from([existing.clone()]));
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("missing.jpg"));
        assert!(!existing.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn image_entry(name: &str, size: u64, hash: Option<u64>) -> ImageEntry {
        ImageEntry {
            path: PathBuf::from(name),
            dimensions: Some((640, 480)),
            file_size: size,
            extension: "jpg".to_owned(),
            thumbnail: None,
            perceptual_hash: Some(hash),
        }
    }

    #[test]
    fn grid_columns_fit_within_the_available_width() {
        for available_width in [120.0, 240.0, 640.0, 1024.0, 1920.0] {
            for requested_size in [80.0, 160.0, 320.0] {
                let thumbnail_size = fit_thumbnail_size(available_width, requested_size);
                let columns = grid_column_count(available_width, thumbnail_size);
                let cards_width =
                    grid_cards_width(columns, thumbnail_size + CARD_FRAME_HORIZONTAL_MARGIN);

                assert!(
                    cards_width <= available_width - GRID_RIGHT_PADDING,
                    "{columns} cards of width {thumbnail_size} exceed {available_width}px"
                );
            }
        }
    }

    #[test]
    fn narrow_windows_shrink_thumbnails_to_keep_cards_visible() {
        let available_width = 120.0;
        let thumbnail_size = fit_thumbnail_size(available_width, 160.0);
        let card_width = thumbnail_size + CARD_FRAME_HORIZONTAL_MARGIN;

        assert_eq!(grid_column_count(available_width, thumbnail_size), 1);
        assert!(card_width <= available_width - GRID_RIGHT_PADDING);
    }

    #[test]
    fn grid_reduces_columns_when_right_margin_would_be_under_ten_pixels() {
        let card_width = 160.0 + CARD_FRAME_HORIZONTAL_MARGIN;
        let available_width = 3.0 * card_width + 2.0 * GRID_SPACING + GRID_RIGHT_PADDING - 0.5;
        let thumbnail_size = fit_thumbnail_size(available_width, 160.0);
        let columns = grid_column_count(available_width, thumbnail_size);
        let right_margin = available_width
            - grid_cards_width(columns, thumbnail_size + CARD_FRAME_HORIZONTAL_MARGIN);

        assert_eq!(columns, 2);
        assert!(right_margin >= 10.0);
    }

    #[test]
    fn plain_click_replaces_selection_and_control_click_toggles() {
        let order = vec![
            PathBuf::from("a.jpg"),
            PathBuf::from("b.jpg"),
            PathBuf::from("c.jpg"),
        ];
        let mut selected = HashSet::from([PathBuf::from("a.jpg")]);
        let mut anchor = Some(PathBuf::from("a.jpg"));

        select_image(
            &mut selected,
            &mut anchor,
            &order,
            Path::new("b.jpg"),
            false,
            false,
        );
        assert_eq!(selected, HashSet::from([PathBuf::from("b.jpg")]));

        select_image(
            &mut selected,
            &mut anchor,
            &order,
            Path::new("c.jpg"),
            false,
            true,
        );
        assert_eq!(
            selected,
            HashSet::from([PathBuf::from("b.jpg"), PathBuf::from("c.jpg")])
        );

        select_image(
            &mut selected,
            &mut anchor,
            &order,
            Path::new("b.jpg"),
            false,
            true,
        );
        assert_eq!(selected, HashSet::from([PathBuf::from("c.jpg")]));
    }

    #[test]
    fn shift_click_selects_a_range_and_control_shift_adds_a_range() {
        let order = vec![
            PathBuf::from("a.jpg"),
            PathBuf::from("b.jpg"),
            PathBuf::from("c.jpg"),
            PathBuf::from("d.jpg"),
        ];
        let mut selected = HashSet::from([PathBuf::from("a.jpg")]);
        let mut anchor = Some(PathBuf::from("a.jpg"));

        select_image(
            &mut selected,
            &mut anchor,
            &order,
            Path::new("c.jpg"),
            true,
            false,
        );
        assert_eq!(
            selected,
            HashSet::from([
                PathBuf::from("a.jpg"),
                PathBuf::from("b.jpg"),
                PathBuf::from("c.jpg"),
            ])
        );

        select_image(
            &mut selected,
            &mut anchor,
            &order,
            Path::new("d.jpg"),
            true,
            true,
        );
        assert_eq!(
            selected,
            HashSet::from([
                PathBuf::from("a.jpg"),
                PathBuf::from("b.jpg"),
                PathBuf::from("c.jpg"),
                PathBuf::from("d.jpg"),
            ])
        );
    }

    #[test]
    fn similar_sizes_are_grouped_within_ten_percent() {
        let images = vec![
            image_entry("a.jpg", 1000, None),
            image_entry("b.jpg", 1090, None),
            image_entry("c.jpg", 1300, None),
        ];

        let groups = similar_size_groups(&images);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].1, vec![0, 1]);
        assert_eq!(groups[1].1, vec![2]);
    }

    #[test]
    fn similar_visual_hashes_are_grouped_but_unrelated_images_are_not() {
        let images = vec![
            image_entry("a.jpg", 1000, Some(0)),
            image_entry("b.jpg", 1000, Some(1)),
            image_entry("c.jpg", 1000, Some(u64::MAX)),
        ];

        let groups = duplicate_groups(&images);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, vec![0, 1]);
    }

    #[test]
    fn destination_collision_gets_a_new_name() {
        let root = temp_test_root("collision");
        let destination = root.join("destination");
        fs::create_dir_all(&destination).unwrap();
        let source = root.join("photo.jpg");
        fs::write(&source, b"source").unwrap();
        fs::write(destination.join("photo.jpg"), b"existing").unwrap();

        let (target, _file) = create_unique_target(&source, &destination).unwrap();

        assert_eq!(target.file_name().unwrap(), "photo (copy 1).jpg");
        drop(_file);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_preserves_existing_destination_files() {
        let root = temp_test_root("copy");
        let source_dir = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let source = source_dir.join("photo.jpg");
        fs::write(&source, b"source").unwrap();
        fs::write(destination.join("photo.jpg"), b"existing").unwrap();

        copy_to_directory(&source, &destination).unwrap();

        assert_eq!(
            fs::read(destination.join("photo.jpg")).unwrap(),
            b"existing"
        );
        assert_eq!(
            fs::read(destination.join("photo (copy 1).jpg")).unwrap(),
            b"source"
        );
        assert!(source.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_preserves_existing_destination_files() {
        let root = temp_test_root("move");
        let source_dir = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&destination).unwrap();
        let source = source_dir.join("photo.jpg");
        fs::write(&source, b"source").unwrap();
        fs::write(destination.join("photo.jpg"), b"existing").unwrap();

        move_to_directory(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read(destination.join("photo.jpg")).unwrap(),
            b"existing"
        );
        assert_eq!(
            fs::read(destination.join("photo (copy 1).jpg")).unwrap(),
            b"source"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
