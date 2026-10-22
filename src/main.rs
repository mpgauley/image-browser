use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

const THUMBNAIL_MAX_EDGE: u32 = 512;
const PREVIEW_MAX_EDGE: u32 = 2048;
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
    FileType,
}

impl GroupMode {
    fn label(self) -> &'static str {
        match self {
            Self::None => "All images",
            Self::Dimensions => "Dimensions",
            Self::SimilarSize => "Similar file size",
            Self::FileType => "File type",
        }
    }
}

struct ImageEntry {
    path: PathBuf,
    dimensions: Option<(u32, u32)>,
    dimensions_loaded: bool,
    file_size: u64,
    extension: String,
    thumbnail: Option<TextureHandle>,
    thumbnail_loaded: bool,
    thumbnail_error: Option<String>,
}

struct ScannedImage {
    path: PathBuf,
    file_size: u64,
    extension: String,
}

struct GroupingImage {
    path: PathBuf,
    dimensions: Option<(u32, u32)>,
    dimensions_loaded: bool,
    file_size: u64,
    extension: String,
}

struct GroupingResult {
    groups: ImageGroups,
    dimensions: Vec<(usize, Option<(u32, u32)>)>,
    failed: usize,
}

type ImageGroups = Vec<(String, Vec<usize>)>;

enum BackgroundOutput {
    FolderScan(Result<(Vec<ScannedImage>, usize), String>),
    Grouping(GroupingResult),
}

enum BackgroundEvent {
    Progress { completed: usize, total: usize },
    Finished(BackgroundOutput),
}

struct BackgroundTask {
    receiver: Receiver<BackgroundEvent>,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
    pending_output: Option<BackgroundOutput>,
}

struct TaskProgress {
    label: String,
    completed: usize,
    total: usize,
}

struct SelectedFileMetadata {
    path: PathBuf,
    file_type: String,
    dimensions: String,
    size: String,
    created: String,
    modified: String,
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
    all_images_group: Arc<ImageGroups>,
    grouped_images: Option<Arc<ImageGroups>>,
    background_task: Option<BackgroundTask>,
    progress: Option<TaskProgress>,
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
            all_images_group: Arc::new(Vec::new()),
            grouped_images: None,
            background_task: None,
            progress: None,
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
        let directory = directory.to_path_buf();
        self.cancel_background_task();
        self.grouped_images = None;
        self.status = "Opening folder…".to_owned();
        self.start_background_task("Opening folder", move |sender, cancel| {
            let result = scan_directory_with_progress(
                &directory,
                |completed, total| {
                    send_progress(&sender, completed, total, &cancel);
                },
                || cancel.load(Ordering::Relaxed),
            )
            .map_err(|error| format!("{error:#}"));
            BackgroundOutput::FolderScan(result)
        });
    }

    fn set_directory(&mut self, directory: PathBuf) {
        self.path_input = directory.display().to_string();
        self.current_dir = Some(directory);
        self.refresh();
    }

    fn set_group_mode(&mut self, mode: GroupMode) {
        self.group_mode = mode;
        self.cancel_background_task();
        if mode == GroupMode::None {
            self.grouped_images = None;
            self.status = format!("Showing {} images.", self.images.len());
            return;
        }
        self.start_grouping(mode);
    }

    fn start_grouping(&mut self, mode: GroupMode) {
        let images = self
            .images
            .iter()
            .map(|entry| GroupingImage {
                path: entry.path.clone(),
                dimensions: entry.dimensions,
                dimensions_loaded: entry.dimensions_loaded,
                file_size: entry.file_size,
                extension: entry.extension.clone(),
            })
            .collect();
        self.grouped_images = None;
        self.status = format!("Grouping images by {}…", mode.label().to_lowercase());
        self.start_background_task(mode.label(), move |sender, cancel| {
            BackgroundOutput::Grouping(group_images(images, mode, sender, cancel))
        });
    }

    fn cancel_background_task(&mut self) {
        if let Some(task) = self.background_task.take() {
            task.cancel.store(true, Ordering::Relaxed);
        }
        self.progress = None;
    }

    fn start_background_task(
        &mut self,
        label: &str,
        work: impl FnOnce(Sender<BackgroundEvent>, Arc<AtomicBool>) -> BackgroundOutput + Send + 'static,
    ) {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let output = work(sender.clone(), Arc::clone(&worker_cancel));
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = sender.send(BackgroundEvent::Finished(output));
            }
        });
        self.progress = Some(TaskProgress {
            label: label.to_owned(),
            completed: 0,
            total: 0,
        });
        self.background_task = Some(BackgroundTask {
            receiver,
            cancel,
            started_at: Instant::now(),
            pending_output: None,
        });
    }

    fn poll_background_task(&mut self, ctx: &egui::Context) {
        let Some(task) = self.background_task.as_ref() else {
            return;
        };
        if task.pending_output.is_some() {
            if task.started_at.elapsed() < Duration::from_millis(150) {
                ctx.request_repaint_after(Duration::from_millis(40));
                return;
            }
            let output = self
                .background_task
                .as_mut()
                .and_then(|task| task.pending_output.take())
                .expect("pending task output was checked");
            self.background_task = None;
            self.progress = None;
            self.apply_background_output(output);
            return;
        }
        let (events, disconnected) = {
            let mut events = Vec::new();
            let mut disconnected = false;
            loop {
                match task.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            (events, disconnected)
        };
        let mut finished_output = None;
        for event in events {
            match event {
                BackgroundEvent::Progress { completed, total } => {
                    if let Some(progress) = &mut self.progress {
                        progress.completed = completed;
                        progress.total = total;
                    }
                }
                BackgroundEvent::Finished(output) => {
                    let task = self
                        .background_task
                        .as_mut()
                        .expect("background task is still active");
                    if task.started_at.elapsed() < Duration::from_millis(150) {
                        task.pending_output = Some(output);
                    } else {
                        finished_output = Some(output);
                    }
                }
            }
        }
        if let Some(output) = finished_output {
            self.background_task = None;
            self.progress = None;
            self.apply_background_output(output);
        } else if disconnected
            && !self
                .background_task
                .as_ref()
                .is_some_and(|task| task.pending_output.is_some())
        {
            self.background_task = None;
            self.progress = None;
            self.status = "Background task ended unexpectedly.".to_owned();
        }
        if self.background_task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }

    fn apply_background_output(&mut self, output: BackgroundOutput) {
        match output {
            BackgroundOutput::FolderScan(Ok((scanned, skipped))) => {
                let count = scanned.len();
                self.images = scanned
                    .into_iter()
                    .map(|image| ImageEntry {
                        path: image.path,
                        dimensions: None,
                        dimensions_loaded: false,
                        file_size: image.file_size,
                        extension: image.extension,
                        thumbnail: None,
                        thumbnail_loaded: false,
                        thumbnail_error: None,
                    })
                    .collect();
                self.selected.clear();
                self.selection_anchor = None;
                self.all_images_group = Arc::new(vec![("Images".to_owned(), (0..count).collect())]);
                self.grouped_images = None;
                if self.group_mode != GroupMode::None {
                    self.status = if skipped > 0 {
                        format!(
                            "Loaded {count} images; skipped {skipped} unreadable file(s). Grouping…"
                        )
                    } else {
                        format!("Loaded {count} images. Grouping…")
                    };
                    self.start_grouping(self.group_mode);
                } else {
                    self.status = if skipped > 0 {
                        format!("Loaded {count} images; skipped {skipped} unreadable file(s).")
                    } else {
                        format!("Loaded {count} images.")
                    };
                }
            }
            BackgroundOutput::FolderScan(Err(error)) => {
                self.status = format!("Could not open folder: {error}");
            }
            BackgroundOutput::Grouping(result) => {
                for (index, dimensions) in result.dimensions {
                    if let Some(entry) = self.images.get_mut(index) {
                        entry.dimensions = dimensions;
                        entry.dimensions_loaded = true;
                    }
                }
                self.grouped_images = Some(Arc::new(result.groups));
                self.status = if result.failed > 0 {
                    format!(
                        "Grouping complete; could not analyze {} image(s).",
                        result.failed
                    )
                } else {
                    format!(
                        "Grouping by {} complete.",
                        self.group_mode.label().to_lowercase()
                    )
                };
            }
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

    fn image_groups(&self) -> Arc<ImageGroups> {
        if self.group_mode == GroupMode::None {
            Arc::clone(&self.all_images_group)
        } else {
            self.grouped_images
                .as_ref()
                .cloned()
                .unwrap_or_else(|| Arc::new(Vec::new()))
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
                let folder_scanning = self
                    .progress
                    .as_ref()
                    .is_some_and(|progress| progress.label == "Opening folder");
                ui.add_enabled_ui(!folder_scanning, |ui| {
                    for mode in [
                        GroupMode::None,
                        GroupMode::Dimensions,
                        GroupMode::SimilarSize,
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
                        ui.label(if self.background_task.is_some() {
                            "Opening folder…"
                        } else {
                            "No supported images in this folder."
                        });
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
                for (group_name, image_indices) in groups.iter() {
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
                        for (position, &index) in image_indices.iter().enumerate() {
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

    fn draw_selected_metadata(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("selected-metadata")
            .resizable(true)
            .default_height(164.0)
            .min_height(88.0)
            .max_height(280.0)
            .show(ctx, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.heading(format!("Selected file metadata ({})", self.selected.len()));
                    if self.selected.is_empty() {
                        ui.label("Select an image to view its metadata.");
                        return;
                    }

                    let mut selected_paths: Vec<_> = self.selected.iter().cloned().collect();
                    selected_paths.sort();
                    egui::ScrollArea::vertical()
                        .max_height(ui.available_height())
                        .show(ui, |ui| {
                            egui::Grid::new("selected-file-metadata")
                                .num_columns(2)
                                .striped(true)
                                .show(ui, |ui| {
                                    for path in selected_paths {
                                        let Some(entry) =
                                            self.images.iter_mut().find(|entry| entry.path == path)
                                        else {
                                            continue;
                                        };
                                        let metadata = selected_file_metadata(entry);
                                        ui.strong(
                                            metadata
                                                .path
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy(),
                                        );
                                        ui.label(&metadata.file_type);
                                        ui.end_row();
                                        metadata_row(ui, "Dimensions", &metadata.dimensions);
                                        metadata_row(ui, "Size", &metadata.size);
                                        metadata_row(ui, "Date created", &metadata.created);
                                        metadata_row(ui, "Date modified", &metadata.modified);
                                        ui.end_row();
                                    }
                                });
                        });
                });
            });
    }

    fn draw_status_panel(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if !self.images.is_empty() {
                    ui.separator();
                    ui.label(format!("{} images", self.images.len()));
                }
                if let Some(progress) = &self.progress {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(format!("{} / {}", progress.completed, progress.total));
                        let fraction = if progress.total == 0 {
                            0.0
                        } else {
                            progress.completed as f32 / progress.total as f32
                        };
                        let bar = egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                            .desired_width(180.0)
                            .text(if progress.total == 0 {
                                format!("{}…", progress.label)
                            } else {
                                format!("{} ({:.0}%)", progress.label, fraction * 100.0)
                            });
                        ui.add(if progress.total == 0 {
                            bar.animate(true)
                        } else {
                            bar
                        });
                    });
                }
            });
        });
    }
}

impl eframe::App for ImageBrowserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background_task(ctx);
        self.draw_top_panel(ctx);
        self.draw_status_panel(ctx);
        self.draw_selected_metadata(ctx);
        self.draw_image_grid(ctx);
        self.draw_preview(ctx);
        self.draw_delete_confirmation(ctx);
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

fn scan_directory_with_progress(
    directory: &Path,
    mut report_progress: impl FnMut(usize, usize),
    is_cancelled: impl Fn() -> bool,
) -> Result<(Vec<ScannedImage>, usize)> {
    let read_dir =
        fs::read_dir(directory).with_context(|| format!("reading {}", directory.display()))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        if is_cancelled() {
            return Ok((Vec::new(), 0));
        }
        entries.push(entry);
    }
    let total = entries.len();
    report_progress(0, total);
    let mut images = Vec::new();
    let mut skipped = 0;
    let mut completed = 0;

    for entry in entries {
        if is_cancelled() {
            break;
        }
        completed += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                if completed % 16 == 0 || completed == total {
                    report_progress(completed, total);
                }
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            if completed % 16 == 0 || completed == total {
                report_progress(completed, total);
            }
            continue;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !supported_extension(&extension) {
            if completed % 16 == 0 || completed == total {
                report_progress(completed, total);
            }
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped += 1;
                if completed % 16 == 0 || completed == total {
                    report_progress(completed, total);
                }
                continue;
            }
        };
        images.push(ScannedImage {
            path,
            file_size: metadata.len(),
            extension,
        });
        if completed % 16 == 0 || completed == total {
            report_progress(completed, total);
        }
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
            ui.allocate_ui_with_layout(
                thumbnail_area,
                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    if !entry.thumbnail_loaded && ui.is_rect_visible(ui.max_rect()) {
                        entry.thumbnail_loaded = true;
                        match load_thumbnail(ctx, &entry.path, THUMBNAIL_MAX_EDGE) {
                            Ok((texture, dimensions)) => {
                                entry.thumbnail = Some(texture);
                                entry.dimensions = Some(dimensions);
                                entry.dimensions_loaded = true;
                            }
                            Err(error) => {
                                entry.dimensions_loaded = true;
                                entry.thumbnail_error = Some(format!("{error:#}"));
                            }
                        }
                    }
                    if let Some(texture) = &entry.thumbnail {
                        let texture_size = texture.size_vec2();
                        let factor =
                            (thumbnail_size / texture_size.x).min(thumbnail_size / texture_size.y);
                        let size = texture_size * factor;
                        let response = ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .sense(egui::Sense::click()),
                        );
                        clicked = response.clicked();
                        double_clicked = response.double_clicked();
                    } else if entry.thumbnail_loaded {
                        let error = entry.thumbnail_error.as_deref().unwrap_or("Unknown error");
                        ui.label("Preview unavailable").on_hover_text(error);
                    }
                },
            );
        });
    (clicked, double_clicked)
}

fn load_texture(ctx: &egui::Context, path: &Path, max_edge: u32) -> Result<TextureHandle> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    texture_from_image(ctx, path, &image, max_edge)
}

fn load_thumbnail(
    ctx: &egui::Context,
    path: &Path,
    max_edge: u32,
) -> Result<(TextureHandle, (u32, u32))> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    let dimensions = (image.width(), image.height());
    let texture = texture_from_image(ctx, path, &image, max_edge)?;
    Ok((texture, dimensions))
}

fn texture_from_image(
    ctx: &egui::Context,
    path: &Path,
    image: &image::DynamicImage,
    max_edge: u32,
) -> Result<TextureHandle> {
    let thumbnail = image.thumbnail(max_edge, max_edge).to_rgba8();
    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
    let color_image = ColorImage::from_rgba_unmultiplied(size, thumbnail.as_raw());
    Ok(ctx.load_texture(
        path.display().to_string(),
        color_image,
        TextureOptions::LINEAR,
    ))
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

fn send_progress(
    sender: &Sender<BackgroundEvent>,
    completed: usize,
    total: usize,
    cancel: &AtomicBool,
) {
    if !cancel.load(Ordering::Relaxed) {
        let _ = sender.send(BackgroundEvent::Progress { completed, total });
    }
}

fn group_images(
    mut images: Vec<GroupingImage>,
    mode: GroupMode,
    sender: Sender<BackgroundEvent>,
    cancel: Arc<AtomicBool>,
) -> GroupingResult {
    let count = images.len();
    let mut dimensions = Vec::new();
    let mut failed = 0;

    let groups = match mode {
        GroupMode::None => Vec::new(),
        GroupMode::Dimensions => {
            send_progress(&sender, 0, count, &cancel);
            for (index, entry) in images.iter_mut().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if !entry.dimensions_loaded {
                    entry.dimensions = image::image_dimensions(&entry.path).ok();
                    entry.dimensions_loaded = true;
                    if entry.dimensions.is_none() {
                        failed += 1;
                    }
                    dimensions.push((index, entry.dimensions));
                }
                if index % 8 == 0 || index + 1 == count {
                    send_progress(&sender, index + 1, count, &cancel);
                }
            }
            group_by_key(&images, |entry| match entry.dimensions {
                Some((width, height)) => format!("{width} × {height}"),
                None => "Unknown dimensions".to_owned(),
            })
        }
        GroupMode::FileType => {
            send_progress(&sender, 0, count, &cancel);
            let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for (index, entry) in images.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let key = if entry.extension.is_empty() {
                    "Unknown file type".to_owned()
                } else {
                    format!(".{}", entry.extension.to_uppercase())
                };
                grouped.entry(key).or_default().push(index);
                if index % 16 == 0 || index + 1 == count {
                    send_progress(&sender, index + 1, count, &cancel);
                }
            }
            grouped.into_iter().collect()
        }
        GroupMode::SimilarSize => similar_size_groups(&images, &sender, &cancel),
    };

    GroupingResult {
        groups,
        dimensions,
        failed,
    }
}

fn selected_file_metadata(entry: &mut ImageEntry) -> SelectedFileMetadata {
    if !entry.dimensions_loaded {
        entry.dimensions = image::image_dimensions(&entry.path).ok();
        entry.dimensions_loaded = true;
    }
    let metadata = fs::metadata(&entry.path);
    let created = metadata
        .as_ref()
        .ok()
        .and_then(|metadata| metadata.created().ok())
        .map(format_system_time)
        .unwrap_or_else(|| "Unavailable".to_owned());
    let modified = metadata
        .as_ref()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(format_system_time)
        .unwrap_or_else(|| "Unavailable".to_owned());
    let file_type = if entry.extension.is_empty() {
        "Unknown".to_owned()
    } else {
        format!(".{}", entry.extension.to_uppercase())
    };
    let dimensions = entry
        .dimensions
        .map(|(width, height)| format!("{width} × {height}"))
        .unwrap_or_else(|| "Unavailable".to_owned());

    SelectedFileMetadata {
        path: entry.path.clone(),
        file_type,
        dimensions,
        size: format_file_size(entry.file_size),
        created,
        modified,
    }
}

fn format_system_time(timestamp: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(timestamp)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn metadata_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(label);
    ui.label(value);
}

fn group_by_key(images: &[GroupingImage], key: impl Fn(&GroupingImage) -> String) -> ImageGroups {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, entry) in images.iter().enumerate() {
        groups.entry(key(entry)).or_default().push(index);
    }
    groups.into_iter().collect()
}

fn similar_size_groups(
    images: &[GroupingImage],
    sender: &Sender<BackgroundEvent>,
    cancel: &AtomicBool,
) -> ImageGroups {
    let mut sorted: Vec<_> = (0..images.len()).collect();
    send_progress(sender, 0, images.len(), cancel);
    sorted.sort_by_key(|&index| images[index].file_size);

    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut anchor = 0;
    let count = sorted.len();
    for (position, index) in sorted.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
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
        if position % 16 == 0 || position + 1 == count {
            send_progress(sender, position + 1, count, cancel);
        }
    }
    if !current.is_empty() {
        groups.push(size_group(images, current));
    }
    groups
}

fn size_group(images: &[GroupingImage], indices: Vec<usize>) -> (String, Vec<usize>) {
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

    fn image_entry(name: &str, size: u64) -> GroupingImage {
        GroupingImage {
            path: PathBuf::from(name),
            dimensions: Some((640, 480)),
            dimensions_loaded: true,
            file_size: size,
            extension: "jpg".to_owned(),
        }
    }

    #[test]
    fn scanning_folder_defers_image_decoding_until_thumbnail_is_visible() {
        let root = temp_test_root("lazy-thumbnail");
        let path = root.join("photo.png");
        image::RgbaImage::new(12, 8).save(&path).unwrap();

        let mut progress = Vec::new();
        let (images, skipped) = scan_directory_with_progress(
            &root,
            |completed, total| {
                progress.push((completed, total));
            },
            || false,
        )
        .unwrap();

        assert_eq!(skipped, 0);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].path, path);
        assert_eq!(images[0].file_size, fs::metadata(&path).unwrap().len());
        assert_eq!(progress.last(), Some(&(1, 1)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_file_metadata_includes_type_dimensions_size_and_timestamps() {
        let root = temp_test_root("selected-metadata");
        let path = root.join("photo.png");
        image::RgbaImage::new(12, 8).save(&path).unwrap();
        let file_size = fs::metadata(&path).unwrap().len();
        let mut entry = ImageEntry {
            path: path.clone(),
            dimensions: None,
            dimensions_loaded: false,
            file_size,
            extension: "png".to_owned(),
            thumbnail: None,
            thumbnail_loaded: false,
            thumbnail_error: None,
        };

        let metadata = selected_file_metadata(&mut entry);

        assert_eq!(metadata.path.file_name().unwrap(), "photo.png");
        assert_eq!(metadata.file_type, ".PNG");
        assert_eq!(metadata.dimensions, "12 × 8");
        assert_eq!(metadata.size, format_file_size(file_size));
        assert!(metadata.modified.ends_with('Z'));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grouping_modes_report_completion_progress() {
        for mode in [
            GroupMode::Dimensions,
            GroupMode::SimilarSize,
            GroupMode::FileType,
        ] {
            let images = vec![image_entry("a.jpg", 1000), image_entry("b.png", 1090)];
            let (sender, receiver) = mpsc::channel();
            let result = group_images(images, mode, sender, Arc::new(AtomicBool::new(false)));
            let progress: Vec<_> = receiver
                .try_iter()
                .filter_map(|event| match event {
                    BackgroundEvent::Progress { completed, total } => Some((completed, total)),
                    BackgroundEvent::Finished(_) => None,
                })
                .collect();

            assert!(
                !result.groups.is_empty(),
                "{} produced no groups",
                mode.label()
            );
            assert!(
                progress
                    .last()
                    .is_some_and(|(completed, total)| completed == total),
                "{} did not reach completion",
                mode.label()
            );
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
            image_entry("a.jpg", 1000),
            image_entry("b.jpg", 1090),
            image_entry("c.jpg", 1300),
        ];

        let (sender, _receiver) = mpsc::channel();
        let groups = similar_size_groups(&images, &sender, &AtomicBool::new(false));

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].1, vec![0, 1]);
        assert_eq!(groups[1].1, vec![2]);
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
