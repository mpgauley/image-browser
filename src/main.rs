use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
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

struct DecodedThumbnail {
    image: ColorImage,
    dimensions: (u32, u32),
}

enum ThumbnailEvent {
    Loaded {
        index: usize,
        result: std::result::Result<DecodedThumbnail, String>,
    },
    Finished,
}

struct ThumbnailTask {
    receiver: Receiver<ThumbnailEvent>,
    cancel: Arc<AtomicBool>,
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
    modified: String,
}

struct Preview {
    path: PathBuf,
    texture: Option<TextureHandle>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesktopEnvironment {
    Gnome,
    Cinnamon,
    KdePlasma,
    Other,
}

impl DesktopEnvironment {
    fn detect() -> Self {
        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_lowercase();
        if desktops.contains("cinnamon") {
            Self::Cinnamon
        } else if desktops.contains("gnome") {
            Self::Gnome
        } else if desktops.contains("kde") || desktops.contains("plasma") {
            Self::KdePlasma
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayInfo {
    name: String,
    plasma_screen: usize,
}

#[derive(Clone)]
enum WallpaperTarget {
    Display(usize),
    Displays(Vec<usize>),
}

enum ContextMenuAction {
    Copy,
    Move,
    Delete,
    SetWallpaper(PathBuf, WallpaperTarget),
    ChooseMultipleDisplays(PathBuf),
    ChooseSlideshow(Vec<PathBuf>),
    StopSlideshow,
}

struct CardContext<'a> {
    selected: bool,
    selected_count: usize,
    selected_paths: &'a [PathBuf],
    desktop_environment: DesktopEnvironment,
    displays: &'a [DisplayInfo],
    display_detection_error: Option<&'a str>,
}

struct WallpaperDisplayDialog {
    path: PathBuf,
    selected: HashSet<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SlideshowInterval {
    unit: String,
    value: u64,
    label: String,
}

struct SlideshowSetupDialog {
    paths: Vec<PathBuf>,
    interval_index: usize,
}

#[derive(serde::Serialize)]
struct SlideshowConfig {
    desktop: &'static str,
    interval_unit: String,
    interval_value: u64,
    paths_hex: Vec<String>,
    plasma_screens: Vec<usize>,
}

const SLIDESHOW_WORKER: &str = r#"#!/usr/bin/python3
import datetime
import json
import os
import subprocess
import time
import urllib.parse

CONFIG_PATH = os.path.join(os.path.dirname(__file__), "config.json")

def read_config():
    with open(CONFIG_PATH, "r", encoding="utf-8") as stream:
        config = json.load(stream)
    config["paths"] = [os.fsdecode(bytes.fromhex(path)) for path in config["paths_hex"]]
    return config

def image_slot(now, unit, value):
    epoch = int(now.timestamp())
    if unit in ("seconds", "minutes", "hours", "days", "weeks"):
        factor = {"seconds": 1, "minutes": 60, "hours": 3600,
                  "days": 86400, "weeks": 604800}[unit] * value
        if unit == "weeks":
            epoch -= 345600
        slot = epoch // factor
        boundary_offset = 345600 if unit == "weeks" else 0
        return slot, (slot + 1) * factor + boundary_offset
    if unit == "months":
        month = (now.year - 1970) * 12 + now.month - 1
        slot = month // value
        next_month = (slot + 1) * value
        year, month_index = divmod(next_month, 12)
        boundary = datetime.datetime(1970 + year, month_index + 1, 1,
                                    tzinfo=datetime.timezone.utc)
        return slot, int(boundary.timestamp())
    year = now.year - 1970
    slot = year // value
    boundary_year = 1970 + (slot + 1) * value
    boundary = datetime.datetime(boundary_year, 1, 1,
                                 tzinfo=datetime.timezone.utc)
    return slot, int(boundary.timestamp())

def set_wallpaper(config, path):
    desktop = config["desktop"]
    if desktop in ("gnome", "cinnamon"):
        schema = ("org.gnome.desktop.background" if desktop == "gnome"
                  else "org.cinnamon.desktop.background")
        uri = "file://" + urllib.parse.quote(path, safe="/")
        subprocess.run(["gsettings", "set", schema, "picture-uri", uri],
                       check=True)
        if desktop == "gnome":
            subprocess.run(["gsettings", "set", schema, "picture-uri-dark", uri],
                           check=True)
        return
    uri = "file://" + urllib.parse.quote(path, safe="/")
    screens = ",".join(str(screen) for screen in config["plasma_screens"])
    script = ("var selectedScreens = [" + screens + "]; "
              "for (var key in desktops()) { var d = desktops()[key]; "
              "if (selectedScreens.indexOf(d.screen) >= 0) { "
              "d.wallpaperPlugin = 'org.kde.image'; "
              "d.currentConfigGroup = ['Wallpaper', 'org.kde.image', 'General']; "
              "d.writeConfig('Image', '" + uri + "'); "
              "d.writeConfig('FillMode', 2); } }")
    last_error = None
    for executable in ("qdbus6", "qdbus"):
        try:
            subprocess.run([executable, "org.kde.plasmashell", "/PlasmaShell",
                            "org.kde.PlasmaShell.evaluateScript", script],
                           check=True, stdout=subprocess.DEVNULL)
            return
        except (OSError, subprocess.CalledProcessError) as error:
            last_error = error
    raise RuntimeError("could not set the Plasma wallpaper: " + str(last_error))

def main():
    while True:
        config = read_config()
        if not config["paths"]:
            raise RuntimeError("slideshow has no images")
        now = datetime.datetime.now(datetime.timezone.utc)
        slot, next_boundary = image_slot(
            now, config["interval_unit"], config["interval_value"])
        image = config["paths"][slot % len(config["paths"])]
        set_wallpaper(config, image)
        delay = max(0.1, next_boundary - time.time())
        time.sleep(delay)

if __name__ == "__main__":
    main()
"#;

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
    thumbnail_task: Option<ThumbnailTask>,
    progress: Option<TaskProgress>,
    preview: Option<Preview>,
    desktop_environment: DesktopEnvironment,
    displays: Vec<DisplayInfo>,
    display_detection_error: Option<String>,
    wallpaper_display_dialog: Option<WallpaperDisplayDialog>,
    slideshow_setup_dialog: Option<SlideshowSetupDialog>,
    status: String,
}

impl ImageBrowserApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let current_dir = default_directory();
        let desktop_environment = DesktopEnvironment::detect();
        let (displays, display_detection_error) = detect_displays(desktop_environment).map_or_else(
            |error| (Vec::new(), Some(format!("{error:#}"))),
            |displays| (displays, None),
        );
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
            thumbnail_task: None,
            progress: None,
            preview: None,
            desktop_environment,
            displays,
            display_detection_error,
            wallpaper_display_dialog: None,
            slideshow_setup_dialog: None,
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
        self.cancel_thumbnail_task();
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
        self.cancel_thumbnail_task();
        self.group_mode = mode;
        self.cancel_background_task();
        if mode == GroupMode::None {
            self.grouped_images = None;
            self.status = format!("Showing {} images.", self.images.len());
            self.start_thumbnail_loading();
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

    fn cancel_thumbnail_task(&mut self) {
        if let Some(task) = self.thumbnail_task.take() {
            task.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn start_thumbnail_loading(&mut self) {
        self.cancel_thumbnail_task();
        let groups = self.image_groups();
        let pending = pending_thumbnail_jobs(&self.images, &groups);
        if pending.is_empty() {
            return;
        }

        let (sender, receiver) = mpsc::sync_channel(2);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            for (index, path) in pending {
                if worker_cancel.load(Ordering::Relaxed) {
                    return;
                }
                let result = load_thumbnail_image(&path, THUMBNAIL_MAX_EDGE)
                    .map_err(|error| format!("{error:#}"));
                if !send_thumbnail_event(
                    &sender,
                    ThumbnailEvent::Loaded { index, result },
                    &worker_cancel,
                ) {
                    return;
                }
            }
            let _ = send_thumbnail_event(&sender, ThumbnailEvent::Finished, &worker_cancel);
        });
        self.thumbnail_task = Some(ThumbnailTask { receiver, cancel });
    }

    fn poll_thumbnail_task(&mut self, ctx: &egui::Context) {
        let Some(task) = self.thumbnail_task.as_ref() else {
            return;
        };
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

        let mut finished = false;
        for event in events {
            match event {
                ThumbnailEvent::Loaded { index, result } => {
                    if let Some(entry) = self.images.get_mut(index) {
                        entry.thumbnail_loaded = true;
                        match result {
                            Ok(decoded) => {
                                entry.dimensions = Some(decoded.dimensions);
                                entry.dimensions_loaded = true;
                                entry.thumbnail = Some(ctx.load_texture(
                                    entry.path.display().to_string(),
                                    decoded.image,
                                    TextureOptions::LINEAR,
                                ));
                            }
                            Err(error) => {
                                entry.dimensions_loaded = true;
                                entry.thumbnail_error = Some(error);
                            }
                        }
                    }
                }
                ThumbnailEvent::Finished => finished = true,
            }
        }
        if finished || disconnected {
            if disconnected && !finished {
                self.status = "Thumbnail loading ended unexpectedly.".to_owned();
            }
            self.thumbnail_task = None;
        } else if self.thumbnail_task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
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
                    self.start_thumbnail_loading();
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
                self.start_thumbnail_loading();
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
            self.remove_images_from_view(&moved_paths);
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
        self.remove_images_from_view(&deleted);
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

    fn remove_images_from_view(&mut self, removed_paths: &HashSet<PathBuf>) {
        if removed_paths.is_empty() {
            return;
        }
        let mut index_mapping = vec![None; self.images.len()];
        let mut remaining =
            Vec::with_capacity(self.images.len().saturating_sub(removed_paths.len()));
        for (old_index, entry) in self.images.drain(..).enumerate() {
            if !removed_paths.contains(&entry.path) {
                index_mapping[old_index] = Some(remaining.len());
                remaining.push(entry);
            }
        }
        self.images = remaining;
        self.selected.retain(|path| !removed_paths.contains(path));
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|path| removed_paths.contains(path))
        {
            self.selection_anchor = None;
        }
        self.all_images_group = Arc::new(vec![(
            "Images".to_owned(),
            (0..self.images.len()).collect(),
        )]);
        if let Some(groups) = self.grouped_images.as_ref() {
            self.grouped_images = Some(Arc::new(remap_image_groups(
                groups,
                &index_mapping,
                &self.images,
                self.group_mode,
            )));
        }
    }

    fn apply_context_menu_action(&mut self, action: ContextMenuAction) {
        match action {
            ContextMenuAction::Copy => self.copy_or_move_selected(false),
            ContextMenuAction::Move => self.copy_or_move_selected(true),
            ContextMenuAction::Delete => self.confirm_delete = true,
            ContextMenuAction::SetWallpaper(path, target) => {
                match stop_wallpaper_slideshow().and_then(|()| {
                    set_wallpaper(self.desktop_environment, &path, target, &self.displays)
                }) {
                    Ok(()) => {
                        self.status = format!(
                            "Set {} as wallpaper.",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                    Err(error) => self.status = format!("Could not set wallpaper: {error:#}"),
                }
            }
            ContextMenuAction::ChooseMultipleDisplays(path) => {
                self.wallpaper_display_dialog = Some(WallpaperDisplayDialog {
                    path,
                    selected: HashSet::new(),
                });
            }
            ContextMenuAction::ChooseSlideshow(paths) => {
                self.slideshow_setup_dialog = Some(SlideshowSetupDialog {
                    paths,
                    interval_index: 15,
                });
            }
            ContextMenuAction::StopSlideshow => match stop_wallpaper_slideshow() {
                Ok(()) => self.status = "Stopped the wallpaper slideshow.".to_owned(),
                Err(error) => {
                    self.status = format!("Could not stop the wallpaper slideshow: {error:#}")
                }
            },
        }
    }

    fn draw_slideshow_setup_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.slideshow_setup_dialog.as_mut() else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        let interval_options = slideshow_intervals();
        if dialog.interval_index >= interval_options.len() {
            self.slideshow_setup_dialog = None;
            self.status = "The selected slideshow interval is invalid.".to_owned();
            return;
        }
        egui::Window::new("Set wallpaper slideshow")
            .id(egui::Id::new("wallpaper-slideshow-setup"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} images will cycle across all displays.",
                    dialog.paths.len()
                ));
                ui.add(
                    egui::Slider::new(&mut dialog.interval_index, 0..=interval_options.len() - 1)
                        .step_by(1.0)
                        .show_value(false)
                        .text("Change wallpaper every"),
                );
                let interval = &interval_options[dialog.interval_index];
                ui.strong(format!("Current selection: {}", interval.label));
                ui.horizontal(|ui| {
                    if ui.button("Start slideshow").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel {
            open = false;
        }
        if apply {
            let Some(dialog) = self.slideshow_setup_dialog.take() else {
                return;
            };
            let interval = &interval_options[dialog.interval_index];
            match start_wallpaper_slideshow(
                self.desktop_environment,
                interval,
                &dialog.paths,
                &self.displays,
            ) {
                Ok(()) => {
                    self.status = format!(
                        "Started a {}-image wallpaper slideshow, changing every {}.",
                        dialog.paths.len(),
                        interval.label
                    );
                }
                Err(error) => {
                    self.status = format!("Could not start wallpaper slideshow: {error:#}")
                }
            }
        } else if !open {
            self.slideshow_setup_dialog = None;
        }
    }

    fn draw_wallpaper_display_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.wallpaper_display_dialog.as_mut() else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        let file_name = dialog
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        egui::Window::new("Set wallpaper on multiple displays")
            .id(egui::Id::new("wallpaper-display-selection"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Image: {file_name}"));
                ui.label("Select at least two displays.");
                for (index, display) in self.displays.iter().enumerate() {
                    let mut selected = dialog.selected.contains(&index);
                    if ui.checkbox(&mut selected, &display.name).changed() {
                        if selected {
                            dialog.selected.insert(index);
                        } else {
                            dialog.selected.remove(&index);
                        }
                    }
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            dialog.selected.len() >= 2,
                            egui::Button::new("Set wallpaper"),
                        )
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel {
            open = false;
        }
        if apply {
            let Some(dialog) = self.wallpaper_display_dialog.take() else {
                return;
            };
            match set_wallpaper(
                self.desktop_environment,
                &dialog.path,
                WallpaperTarget::Displays(
                    dialog
                        .selected
                        .into_iter()
                        .filter_map(|index| {
                            self.displays
                                .get(index)
                                .map(|display| display.plasma_screen)
                        })
                        .collect(),
                ),
                &self.displays,
            ) {
                Ok(()) => {
                    self.status = "Updated wallpaper on selected displays.".to_owned();
                }
                Err(error) => self.status = format!("Could not set wallpaper: {error:#}"),
            }
        } else if !open {
            self.wallpaper_display_dialog = None;
        }
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
        let mut context_clicked_path = None;
        let mut context_action = None;
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
                let selected_paths: Vec<_> = display_order
                    .iter()
                    .filter(|path| self.selected.contains(*path))
                    .cloned()
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
                            let (clicked, double_clicked, secondary_clicked, action) =
                                draw_image_card(
                                    ui,
                                    entry,
                                    CardContext {
                                        selected,
                                        selected_count: self.selected.len(),
                                        selected_paths: &selected_paths,
                                        desktop_environment: self.desktop_environment,
                                        displays: &self.displays,
                                        display_detection_error: self
                                            .display_detection_error
                                            .as_deref(),
                                    },
                                    thumbnail_size,
                                );
                            if secondary_clicked {
                                context_clicked_path = Some(entry.path.clone());
                            }
                            if action.is_some() {
                                context_action = action;
                            }
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
        if let Some(path) = context_clicked_path
            && !self.selected.contains(&path)
        {
            self.selected.clear();
            self.selected.insert(path.clone());
            self.selection_anchor = Some(path);
        }
        if let Some(action) = context_action {
            self.apply_context_menu_action(action);
        }
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
        self.poll_thumbnail_task(ctx);
        self.draw_top_panel(ctx);
        self.draw_status_panel(ctx);
        self.draw_selected_metadata(ctx);
        self.draw_image_grid(ctx);
        self.draw_preview(ctx);
        self.draw_wallpaper_display_dialog(ctx);
        self.draw_slideshow_setup_dialog(ctx);
        self.draw_wallpaper_display_dialog(ctx);
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
    entry: &mut ImageEntry,
    context: CardContext<'_>,
    thumbnail_size: f32,
) -> (bool, bool, bool, Option<ContextMenuAction>) {
    let mut clicked = false;
    let mut double_clicked = false;
    let mut secondary_clicked = false;
    let mut context_action = None;
    egui::Frame::group(ui.style())
        .fill(if context.selected {
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
                    let response = ui.interact(
                        ui.max_rect(),
                        egui::Id::new(("thumbnail-card", &entry.path)),
                        egui::Sense::click(),
                    );
                    clicked = response.clicked();
                    double_clicked = response.double_clicked();
                    secondary_clicked = response.secondary_clicked();
                    response.context_menu(|ui| {
                        if ui.button("Copy To…").clicked() {
                            context_action = Some(ContextMenuAction::Copy);
                            ui.close_menu();
                        }
                        if ui.button("Move To…").clicked() {
                            context_action = Some(ContextMenuAction::Move);
                            ui.close_menu();
                        }
                        if ui.button("Delete").clicked() {
                            context_action = Some(ContextMenuAction::Delete);
                            ui.close_menu();
                        }

                        let effective_selection_count = if context.selected {
                            context.selected_count
                        } else {
                            1
                        };
                        if effective_selection_count == 1 {
                            ui.separator();
                            match context.desktop_environment {
                                DesktopEnvironment::Gnome | DesktopEnvironment::Cinnamon => {
                                    let label = if context.displays.len() > 1 {
                                        format!(
                                            "Set as wallpaper on all {} displays",
                                            context.displays.len()
                                        )
                                    } else {
                                        "Set as wallpaper".to_owned()
                                    };
                                    if ui.button(label).clicked() {
                                        context_action = Some(ContextMenuAction::SetWallpaper(
                                            entry.path.clone(),
                                            WallpaperTarget::Displays(Vec::new()),
                                        ));
                                        ui.close_menu();
                                    }
                                }
                                DesktopEnvironment::KdePlasma if context.displays.len() == 1 => {
                                    if ui.button("Set as wallpaper").clicked() {
                                        context_action = Some(ContextMenuAction::SetWallpaper(
                                            entry.path.clone(),
                                            WallpaperTarget::Display(0),
                                        ));
                                        ui.close_menu();
                                    }
                                }
                                DesktopEnvironment::KdePlasma if context.displays.len() > 1 => {
                                    ui.menu_button("Set wallpaper on…", |ui| {
                                        for (index, display) in context.displays.iter().enumerate()
                                        {
                                            if ui.button(&display.name).clicked() {
                                                context_action =
                                                    Some(ContextMenuAction::SetWallpaper(
                                                        entry.path.clone(),
                                                        WallpaperTarget::Display(index),
                                                    ));
                                                ui.close_menu();
                                            }
                                        }
                                        ui.separator();
                                        if ui.button("Multiple displays…").clicked() {
                                            context_action =
                                                Some(ContextMenuAction::ChooseMultipleDisplays(
                                                    entry.path.clone(),
                                                ));
                                            ui.close_menu();
                                        }
                                        if ui.button("All displays").clicked() {
                                            context_action = Some(ContextMenuAction::SetWallpaper(
                                                entry.path.clone(),
                                                WallpaperTarget::Displays(
                                                    context
                                                        .displays
                                                        .iter()
                                                        .map(|display| display.plasma_screen)
                                                        .collect(),
                                                ),
                                            ));
                                            ui.close_menu();
                                        }
                                    });
                                }
                                DesktopEnvironment::KdePlasma => {
                                    ui.label("Display detection unavailable").on_hover_text(
                                        context.display_detection_error.unwrap_or(
                                            "Could not enumerate the connected displays.",
                                        ),
                                    );
                                }
                                DesktopEnvironment::Other => {}
                            }
                        } else if !matches!(context.desktop_environment, DesktopEnvironment::Other)
                        {
                            ui.separator();
                            if ui.button("Set slideshow on all displays…").clicked() {
                                let paths = if context.selected {
                                    context.selected_paths.to_vec()
                                } else {
                                    vec![entry.path.clone()]
                                };
                                context_action = Some(ContextMenuAction::ChooseSlideshow(paths));
                                ui.close_menu();
                            }
                        }
                        if !matches!(context.desktop_environment, DesktopEnvironment::Other) {
                            ui.separator();
                            if ui.button("Stop wallpaper slideshow").clicked() {
                                context_action = Some(ContextMenuAction::StopSlideshow);
                                ui.close_menu();
                            }
                        }
                    });
                    if let Some(texture) = &entry.thumbnail {
                        let texture_size = texture.size_vec2();
                        let factor =
                            (thumbnail_size / texture_size.x).min(thumbnail_size / texture_size.y);
                        let size = texture_size * factor;
                        ui.add(egui::Image::new(texture).fit_to_exact_size(size));
                    } else if !entry.thumbnail_loaded {
                        ui.label("Loading thumbnail…");
                    } else {
                        let error = entry.thumbnail_error.as_deref().unwrap_or("Unknown error");
                        ui.label("Preview unavailable").on_hover_text(error);
                    }
                },
            );
        });
    (clicked, double_clicked, secondary_clicked, context_action)
}

fn load_texture(ctx: &egui::Context, path: &Path, max_edge: u32) -> Result<TextureHandle> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    texture_from_image(ctx, path, &image, max_edge)
}

fn detect_displays(desktop: DesktopEnvironment) -> Result<Vec<DisplayInfo>> {
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let mut errors = Vec::new();

    if desktop == DesktopEnvironment::KdePlasma {
        match detect_kscreen_displays() {
            Ok(displays) if !displays.is_empty() => return Ok(displays),
            Ok(_) => errors.push("kscreen-doctor reported no active displays".to_owned()),
            Err(error) => errors.push(format!("kscreen-doctor: {error:#}")),
        }
    }

    if session_type.eq_ignore_ascii_case("wayland")
        && matches!(
            desktop,
            DesktopEnvironment::Gnome | DesktopEnvironment::Cinnamon
        )
    {
        match detect_mutter_displays() {
            Ok(displays) if !displays.is_empty() => return Ok(displays),
            Ok(_) => errors.push("Mutter reported no active displays".to_owned()),
            Err(error) => errors.push(format!("Mutter: {error:#}")),
        }
    }

    if !session_type.eq_ignore_ascii_case("wayland") {
        match detect_xrandr_displays() {
            Ok(displays) if !displays.is_empty() => return Ok(displays),
            Ok(_) => errors.push("xrandr reported no active displays".to_owned()),
            Err(error) => errors.push(format!("xrandr: {error:#}")),
        }
    }

    if session_type.eq_ignore_ascii_case("wayland")
        && !matches!(
            desktop,
            DesktopEnvironment::Gnome | DesktopEnvironment::Cinnamon
        )
    {
        match detect_wlr_displays() {
            Ok(displays) if !displays.is_empty() => return Ok(displays),
            Ok(_) => errors.push("wlr-randr reported no active displays".to_owned()),
            Err(error) => errors.push(format!("wlr-randr: {error:#}")),
        }
    }

    Err(anyhow!("could not detect displays ({})", errors.join("; ")))
}

fn detect_xrandr_displays() -> Result<Vec<DisplayInfo>> {
    let output = Command::new("xrandr")
        .arg("--listmonitors")
        .output()
        .context("running xrandr --listmonitors")?;
    if !output.status.success() {
        return Err(anyhow!(
            "xrandr exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_xrandr_displays(&text))
}

fn parse_xrandr_displays(output: &str) -> Vec<DisplayInfo> {
    output
        .lines()
        .filter(|line| {
            line.trim_start()
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit())
        })
        .filter_map(|line| {
            let (index, fields) = line.trim().split_once(':')?;
            let plasma_screen = index.parse().ok()?;
            let name = fields.split_whitespace().last()?.to_owned();
            Some(DisplayInfo {
                name,
                plasma_screen,
            })
        })
        .collect()
}

fn detect_kscreen_displays() -> Result<Vec<DisplayInfo>> {
    let output = Command::new("kscreen-doctor")
        .arg("-o")
        .output()
        .context("running kscreen-doctor -o")?;
    if !output.status.success() {
        return Err(anyhow!(
            "kscreen-doctor exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_kscreen_displays(&text))
}

fn parse_kscreen_displays(output: &str) -> Vec<DisplayInfo> {
    let mut displays = Vec::new();
    for line in output.lines().filter(|line| line.starts_with("Output:")) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 4 || !fields.contains(&"enabled") || !fields.contains(&"connected") {
            continue;
        }
        let name = fields[2].to_owned();
        displays.push(DisplayInfo {
            name,
            plasma_screen: displays.len(),
        });
    }
    displays
}

fn detect_mutter_displays() -> Result<Vec<DisplayInfo>> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Mutter.DisplayConfig",
            "--object-path",
            "/org/gnome/Mutter/DisplayConfig",
            "--method",
            "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ])
        .output()
        .context("querying Mutter display configuration with gdbus")?;
    if !output.status.success() {
        return Err(anyhow!(
            "gdbus exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_mutter_displays(&text))
}

fn parse_mutter_displays(output: &str) -> Vec<DisplayInfo> {
    let Some(start) = output.find('[') else {
        return Vec::new();
    };
    let mut monitors = Vec::new();
    let mut square_depth = 0;
    let mut paren_depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in output.bytes().enumerate().skip(start) {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'\'' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'\'' => quoted = true,
            b'[' => square_depth += 1,
            b']' => {
                square_depth -= 1;
                if square_depth == 0 {
                    break;
                }
            }
            b'(' if square_depth == 1 && paren_depth == 0 => {
                let tail = &output[index + 1..];
                let Some(name) = tail
                    .trim_start()
                    .strip_prefix('\'')
                    .and_then(|tail| tail.split_once('\'').map(|(name, _)| name))
                else {
                    return Vec::new();
                };
                monitors.push(DisplayInfo {
                    name: name.to_owned(),
                    plasma_screen: monitors.len(),
                });
                paren_depth += 1;
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth -= 1,
            _ => {}
        }
    }
    monitors
}

fn detect_wlr_displays() -> Result<Vec<DisplayInfo>> {
    let output = Command::new("wlr-randr")
        .output()
        .context("running wlr-randr")?;
    if !output.status.success() {
        return Err(anyhow!(
            "wlr-randr exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.split_whitespace().next())
        .enumerate()
        .map(|(plasma_screen, name)| DisplayInfo {
            name: name.to_owned(),
            plasma_screen,
        })
        .collect())
}

fn set_wallpaper(
    desktop: DesktopEnvironment,
    path: &Path,
    target: WallpaperTarget,
    displays: &[DisplayInfo],
) -> Result<()> {
    if !path.is_file() {
        return Err(anyhow!("{} is not a readable file", path.display()));
    }
    match desktop {
        DesktopEnvironment::Gnome => {
            set_gsettings_wallpaper("org.gnome.desktop.background", path, true)
        }
        DesktopEnvironment::Cinnamon => {
            set_gsettings_wallpaper("org.cinnamon.desktop.background", path, false)
        }
        DesktopEnvironment::KdePlasma => {
            let screens = match target {
                WallpaperTarget::Display(index) => vec![
                    displays
                        .get(index)
                        .ok_or_else(|| anyhow!("display {index} is no longer available"))?
                        .plasma_screen,
                ],
                WallpaperTarget::Displays(screens) => screens,
            };
            set_plasma_wallpaper(path, &screens)
        }
        DesktopEnvironment::Other => Err(anyhow!(
            "wallpaper setting is supported on GNOME, Cinnamon, and KDE Plasma"
        )),
    }
}

fn slideshow_intervals() -> Vec<SlideshowInterval> {
    let mut intervals = Vec::with_capacity(48);
    for seconds in (5..=60).step_by(5) {
        intervals.push(SlideshowInterval {
            unit: "seconds".to_owned(),
            value: seconds,
            label: if seconds == 60 {
                "1 minute".to_owned()
            } else {
                format!("{seconds} seconds")
            },
        });
    }
    for minutes in 2..=30 {
        intervals.push(SlideshowInterval {
            unit: "minutes".to_owned(),
            value: minutes,
            label: format!("{minutes} minutes"),
        });
    }
    for (unit, value, label) in [
        ("hours", 1, "1 hour"),
        ("hours", 6, "6 hours"),
        ("hours", 12, "12 hours"),
        ("days", 1, "1 day"),
        ("weeks", 1, "1 week"),
        ("months", 1, "1 month"),
        ("years", 1, "1 year"),
    ] {
        intervals.push(SlideshowInterval {
            unit: unit.to_owned(),
            value,
            label: label.to_owned(),
        });
    }
    intervals
}

fn start_wallpaper_slideshow(
    desktop: DesktopEnvironment,
    interval: &SlideshowInterval,
    paths: &[PathBuf],
    displays: &[DisplayInfo],
) -> Result<()> {
    if paths.len() < 2 {
        return Err(anyhow!("select at least two images for the slideshow"));
    }
    if desktop == DesktopEnvironment::Other {
        return Err(anyhow!(
            "wallpaper slideshows are not supported on this desktop"
        ));
    }
    for path in paths {
        if !path.is_file() {
            return Err(anyhow!("{} is not a readable file", path.display()));
        }
    }
    if desktop == DesktopEnvironment::KdePlasma && displays.is_empty() {
        return Err(anyhow!("could not detect any KDE Plasma displays"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("could not determine the home directory"))?;
    let data_dir = home.join(".local/share/image-browser/slideshow");
    let unit_dir = home.join(".config/systemd/user");
    fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    fs::create_dir_all(&unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;

    let config = SlideshowConfig {
        desktop: match desktop {
            DesktopEnvironment::Gnome => "gnome",
            DesktopEnvironment::Cinnamon => "cinnamon",
            DesktopEnvironment::KdePlasma => "kde",
            DesktopEnvironment::Other => unreachable!(),
        },
        interval_unit: interval.unit.clone(),
        interval_value: interval.value,
        paths_hex: paths.iter().map(|path| path_to_hex(path)).collect(),
        plasma_screens: displays
            .iter()
            .map(|display| display.plasma_screen)
            .collect(),
    };
    let config_json = serde_json::to_vec_pretty(&config).context("serializing slideshow config")?;
    let config_path = data_dir.join("config.json");
    fs::write(&config_path, config_json)
        .with_context(|| format!("writing {}", config_path.display()))?;

    let worker_path = data_dir.join("worker.py");
    fs::write(&worker_path, SLIDESHOW_WORKER)
        .with_context(|| format!("writing {}", worker_path.display()))?;

    let python = find_executable("python3")
        .ok_or_else(|| anyhow!("python3 is required to run the persistent slideshow"))?;
    let service_path = unit_dir.join("image-browser-slideshow.service");
    let service = format!(
        "[Unit]\nDescription=Image Browser wallpaper slideshow\nAfter=graphical-session.target\n\n\
         [Service]\nType=simple\nExecStart={} {}\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(&python.display().to_string()),
        systemd_quote(&worker_path.display().to_string())
    );
    fs::write(&service_path, service)
        .with_context(|| format!("writing {}", service_path.display()))?;

    import_desktop_environment()?;
    systemctl_user(&["daemon-reload"])?;
    systemctl_user(&["enable", "image-browser-slideshow.service"])?;
    systemctl_user(&["restart", "image-browser-slideshow.service"])?;
    if !systemctl_user_succeeded(&["is-active", "--quiet", "image-browser-slideshow.service"]) {
        let status = Command::new("systemctl")
            .args([
                "--user",
                "status",
                "--no-pager",
                "--full",
                "image-browser-slideshow.service",
            ])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        return Err(anyhow!(
            "systemd did not start the slideshow service: {status}"
        ));
    }
    Ok(())
}

fn stop_wallpaper_slideshow() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("could not determine the home directory"))?;
    let unit_dir = home.join(".config/systemd/user");
    let service_path = unit_dir.join("image-browser-slideshow.service");
    if !service_path.exists() {
        return Ok(());
    }
    systemctl_user(&["disable", "--now", "image-browser-slideshow.service"])?;
    fs::remove_file(&service_path)
        .with_context(|| format!("removing {}", service_path.display()))?;
    let data_dir = home.join(".local/share/image-browser/slideshow");
    for name in ["config.json", "worker.py"] {
        let path = data_dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("removing {}", path.display()));
            }
        }
    }
    systemctl_user(&["daemon-reload"])?;
    Ok(())
}

fn import_desktop_environment() -> Result<()> {
    let variables = [
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
    ];
    let present: Vec<_> = variables
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("systemctl");
    command
        .arg("--user")
        .arg("import-environment")
        .args(present);
    run_and_check(&mut command, "importing desktop session environment")
}

fn systemctl_user(args: &[&str]) -> Result<()> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(args);
    run_and_check(
        &mut command,
        &format!("systemctl --user {}", args.join(" ")),
    )
}

fn systemctl_user_succeeded(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

fn find_executable(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn systemd_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn path_to_hex(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn set_gsettings_wallpaper(schema: &str, path: &Path, set_dark_uri: bool) -> Result<()> {
    let uri = file_uri(path)?;
    run_and_check(
        Command::new("gsettings").args(["set", schema, "picture-uri", &uri]),
        "setting the desktop wallpaper",
    )?;
    if set_dark_uri {
        run_and_check(
            Command::new("gsettings").args(["set", schema, "picture-uri-dark", &uri]),
            "setting the dark-theme desktop wallpaper",
        )
        .context("the regular wallpaper was set, but the dark-theme wallpaper failed")?;
    }
    Ok(())
}

fn set_plasma_wallpaper(path: &Path, screens: &[usize]) -> Result<()> {
    if screens.is_empty() {
        return Err(anyhow!("no displays were selected"));
    }
    let uri = file_uri(path)?;
    let selected_screens = screens
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "var selectedScreens = [{selected_screens}]; \
         for (var key in desktops()) {{ \
         var d = desktops()[key]; \
         if (selectedScreens.indexOf(d.screen) >= 0) {{ \
         d.wallpaperPlugin = 'org.kde.image'; \
         d.currentConfigGroup = ['Wallpaper', 'org.kde.image', 'General']; \
         d.writeConfig('Image', '{uri}'); \
         d.writeConfig('FillMode', 2); \
         }} }}"
    );
    let mut last_error = None;
    for executable in ["qdbus6", "qdbus"] {
        match Command::new(executable)
            .args([
                "org.kde.plasmashell",
                "/PlasmaShell",
                "org.kde.PlasmaShell.evaluateScript",
                &script,
            ])
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = Some(format!(
                    "{executable} exited with {}: {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => last_error = Some(format!("{executable}: {error}")),
        }
    }
    Err(anyhow!(
        "could not contact Plasma to set the wallpaper ({})",
        last_error.unwrap_or_else(|| "no qdbus executable found".to_owned())
    ))
}

fn file_uri(path: &Path) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;

    if !path.is_absolute() {
        return Err(anyhow!("wallpaper path must be absolute"));
    }
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(*byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

fn run_and_check(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("running command for {action}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "{action} failed with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn load_thumbnail_image(path: &Path, max_edge: u32) -> Result<DecodedThumbnail> {
    let image = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    let dimensions = (image.width(), image.height());
    let thumbnail = image.thumbnail(max_edge, max_edge).to_rgba8();
    let size = [thumbnail.width() as usize, thumbnail.height() as usize];
    let color_image = ColorImage::from_rgba_unmultiplied(size, thumbnail.as_raw());
    Ok(DecodedThumbnail {
        image: color_image,
        dimensions,
    })
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

fn send_thumbnail_event(
    sender: &SyncSender<ThumbnailEvent>,
    mut event: ThumbnailEvent,
    cancel: &AtomicBool,
) -> bool {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(returned_event)) => {
                event = returned_event;
                thread::sleep(Duration::from_millis(10));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn pending_thumbnail_jobs(images: &[ImageEntry], groups: &ImageGroups) -> Vec<(usize, PathBuf)> {
    groups
        .iter()
        .flat_map(|(_, indices)| indices)
        .filter_map(|&index| {
            let entry = &images[index];
            (!entry.thumbnail_loaded).then(|| (index, entry.path.clone()))
        })
        .collect()
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

fn remap_image_groups(
    groups: &ImageGroups,
    index_mapping: &[Option<usize>],
    images: &[ImageEntry],
    mode: GroupMode,
) -> ImageGroups {
    groups
        .iter()
        .filter_map(|(name, old_indices)| {
            let new_indices: Vec<_> = old_indices
                .iter()
                .filter_map(|&old_index| index_mapping.get(old_index).copied().flatten())
                .collect();
            if new_indices.is_empty() {
                return None;
            }
            let new_name = if mode == GroupMode::SimilarSize {
                let smallest = new_indices
                    .iter()
                    .map(|&index| images[index].file_size)
                    .min()
                    .unwrap_or_default();
                let largest = new_indices
                    .iter()
                    .map(|&index| images[index].file_size)
                    .max()
                    .unwrap_or_default();
                format!(
                    "Similar size: {}–{}",
                    format_file_size(smallest),
                    format_file_size(largest)
                )
            } else {
                name.clone()
            };
            Some((new_name, new_indices))
        })
        .collect()
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
    fn parses_xrandr_monitor_names_and_order() {
        let monitors = parse_xrandr_displays(
            "Monitors: 2\n 0: +*DP-4 2560/697x1440/392+0+0 DP-4\n 1: +HDMI-0 1920/500x1080/300+2560+0 HDMI-0\n",
        );

        assert_eq!(
            monitors,
            vec![
                DisplayInfo {
                    name: "DP-4".to_owned(),
                    plasma_screen: 0,
                },
                DisplayInfo {
                    name: "HDMI-0".to_owned(),
                    plasma_screen: 1,
                }
            ]
        );
    }

    #[test]
    fn parses_only_enabled_connected_kscreen_outputs() {
        let monitors = parse_kscreen_displays(
            "Output: 1 eDP-1 enabled connected priority 1\nOutput: 2 HDMI-1 disabled connected priority 2\nOutput: 3 DP-1 enabled connected priority 3\n",
        );

        assert_eq!(
            monitors,
            vec![
                DisplayInfo {
                    name: "eDP-1".to_owned(),
                    plasma_screen: 0,
                },
                DisplayInfo {
                    name: "DP-1".to_owned(),
                    plasma_screen: 1,
                }
            ]
        );
    }

    #[test]
    fn parses_mutter_monitor_array_without_counting_logical_outputs() {
        let monitors = parse_mutter_displays(
            "(uint32 4, [('DP-1', 'vendor', 'product', 'serial', [('DP-1', 'vendor', 'product', 'serial')], {}), ('HDMI-1', 'vendor', 'product', 'serial', [('HDMI-1', 'vendor', 'product', 'serial')], {})], [(0, 0, 1.0, uint32 0, true, [('DP-1', 'vendor', 'product', 'serial')])], {})",
        );

        assert_eq!(
            monitors
                .iter()
                .map(|monitor| monitor.name.as_str())
                .collect::<Vec<_>>(),
            vec!["DP-1", "HDMI-1"]
        );
    }

    #[test]
    fn file_wallpaper_uri_escapes_path_bytes() {
        let uri = file_uri(Path::new("/home/user/my image#1.png")).unwrap();

        assert_eq!(uri, "file:///home/user/my%20image%231.png");
    }

    #[test]
    fn slideshow_interval_steps_follow_the_requested_schedule() {
        let intervals = slideshow_intervals();

        assert_eq!(intervals.len(), 48);
        assert_eq!(intervals[0].label, "5 seconds");
        assert_eq!(intervals[11].label, "1 minute");
        assert_eq!(intervals[12].label, "2 minutes");
        assert_eq!(intervals[40].label, "30 minutes");
        assert_eq!(intervals[41].label, "1 hour");
        assert_eq!(intervals[42].label, "6 hours");
        assert_eq!(intervals[43].label, "12 hours");
        assert_eq!(intervals[44].label, "1 day");
        assert_eq!(intervals[45].label, "1 week");
        assert_eq!(intervals[46].label, "1 month");
        assert_eq!(intervals[47].label, "1 year");
    }

    #[test]
    fn path_config_encoding_preserves_non_utf8_linux_filenames() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', 0xff,
        ]));

        assert_eq!(path_to_hex(&path), "2f746d702fff");
    }

    #[test]
    fn systemd_exec_quoting_escapes_specifiers_and_quotes() {
        assert_eq!(
            systemd_quote("/home/user/100% \"photos\""),
            "\"/home/user/100%% \\\"photos\\\"\""
        );
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
    fn folder_scanning_does_not_decode_image_pixels() {
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
    fn thumbnail_decoder_returns_original_dimensions_and_bounded_pixels() {
        let root = temp_test_root("thumbnail-decoder");
        let path = root.join("photo.png");
        image::RgbaImage::new(12, 8).save(&path).unwrap();

        let decoded = load_thumbnail_image(&path, 4).unwrap();

        assert_eq!(decoded.dimensions, (12, 8));
        assert_eq!(decoded.image.size, [4, 3]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_file_metadata_includes_type_dimensions_size_and_modified_time() {
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
    fn pending_thumbnail_jobs_follow_display_order_and_skip_loaded_images() {
        let images = ["first.png", "already-loaded.png", "last.png"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| ImageEntry {
                path: PathBuf::from(name),
                dimensions: None,
                dimensions_loaded: false,
                file_size: index as u64,
                extension: "png".to_owned(),
                thumbnail: None,
                thumbnail_loaded: index == 1,
                thumbnail_error: None,
            })
            .collect::<Vec<_>>();
        let groups = vec![
            ("Displayed first".to_owned(), vec![2, 1]),
            ("Displayed second".to_owned(), vec![0]),
        ];

        let jobs = pending_thumbnail_jobs(&images, &groups);

        assert_eq!(
            jobs,
            vec![
                (2, PathBuf::from("last.png")),
                (0, PathBuf::from("first.png"))
            ]
        );
    }

    #[test]
    fn removed_images_do_not_leave_stale_group_indices() {
        let images = (0..3)
            .map(|index| ImageEntry {
                path: PathBuf::from(format!("{index}.png")),
                dimensions: Some((10, 10)),
                dimensions_loaded: true,
                file_size: 100 + index,
                extension: "png".to_owned(),
                thumbnail: None,
                thumbnail_loaded: true,
                thumbnail_error: None,
            })
            .collect::<Vec<_>>();
        let groups = vec![
            ("10 × 10".to_owned(), vec![0, 2]),
            ("Other".to_owned(), vec![1]),
        ];
        let index_mapping = [Some(0), None, Some(1)];
        let remaining = [images[0].path.clone(), images[2].path.clone()];
        let image_refs: Vec<_> = remaining
            .iter()
            .map(|path| ImageEntry {
                path: path.clone(),
                dimensions: Some((10, 10)),
                dimensions_loaded: true,
                file_size: 100,
                extension: "png".to_owned(),
                thumbnail: None,
                thumbnail_loaded: true,
                thumbnail_error: None,
            })
            .collect();

        let remapped =
            remap_image_groups(&groups, &index_mapping, &image_refs, GroupMode::Dimensions);

        assert_eq!(remapped, vec![("10 × 10".to_owned(), vec![0, 1])]);
        assert!(
            remapped
                .iter()
                .flat_map(|(_, indices)| indices)
                .all(|index| *index < image_refs.len())
        );
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
