use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;

use crate::capture::{self, CapturePreview, CaptureSourceInfo};
use crate::config::{
    AppConfig, DEFAULT_AUDIO_EXCLUSIONS, FPS_PRESETS, QUALITY_PRESETS, fps_value, generate_token,
    quality_height,
};
use crate::media::CaptureSettings;
use crate::preferences::{self, UserPreferences};
use crate::server;

const WINDOW_PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

pub fn run(config: AppConfig) -> Result<()> {
    run_internal(config, true)
}

pub fn run_without_preferences(config: AppConfig) -> Result<()> {
    run_internal(config, false)
}

fn run_internal(mut config: AppConfig, load_preferences: bool) -> Result<()> {
    if load_preferences && let Some(saved) = preferences::load() {
        saved.apply_to(&mut config);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 900.0])
            .with_min_inner_size([760.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Instant Local Stream",
        options,
        Box::new(move |_creation_context| Ok(Box::new(HostUi::new(config.clone())))),
    )
    .map_err(|error| anyhow::anyhow!("UI failed: {error}"))
}

struct HostUi {
    config: AppConfig,
    status: String,
    command_tx: mpsc::Sender<UiCommand>,
    event_rx: mpsc::Receiver<UiEvent>,
    control_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<server::ServerCommand>>>>,
    preview_request_tx: mpsc::Sender<PreviewRequest>,
    preview_event_rx: mpsc::Receiver<PreviewEvent>,
    preview_pending: usize,
    source_tab: SourceTab,
    source_selected: bool,
    sources: Vec<CaptureSourceInfo>,
    source_previews: HashMap<String, egui::TextureHandle>,
    test_preview: Option<egui::TextureHandle>,
    last_source_refresh: Option<Instant>,
    last_window_preview_refresh: Option<Instant>,
    source_error: Option<String>,
    last_saved_preferences: Option<UserPreferences>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceTab {
    Displays,
    Windows,
    TestPattern,
}

enum UiCommand {
    StartServer(Box<AppConfig>),
    StartStream(Box<CaptureSettings>),
    StopStream,
    Shutdown,
    LookupPublicIp,
}

enum UiEvent {
    ServerReady(String),
    StreamStarted,
    StreamStopped,
    Failed(String),
    PublicIp(String),
    PublicIpFailed(String),
}

struct PreviewRequest {
    kind: &'static str,
    width: u32,
    height: u32,
    fps: u32,
}

struct PreviewFrame {
    key: String,
    preview: CapturePreview,
}

enum PreviewEvent {
    Updated {
        sources: Vec<CaptureSourceInfo>,
        previews: Vec<PreviewFrame>,
    },
    TestPattern(PreviewFrame),
    Failed(String),
}

impl HostUi {
    fn new(config: AppConfig) -> Self {
        let mut config = config;
        if config.source.kind == "test" {
            if config.quality == "source" {
                config.quality = "1080p".to_owned();
            }
            if config.fps_preset == "source" {
                config.fps_preset = "60".to_owned();
            }
            if config.adaptive_quality_ceiling == "source" {
                config.adaptive_quality_ceiling = "1080p".to_owned();
            }
            if config.adaptive_fps_ceiling == "source" {
                config.adaptive_fps_ceiling = "60".to_owned();
            }
        }
        let source_tab = match config.source.kind.as_str() {
            "window" => SourceTab::Windows,
            "test" => SourceTab::TestPattern,
            _ => SourceTab::Displays,
        };
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (preview_request_tx, preview_request_rx) = mpsc::channel();
        let (preview_event_tx, preview_event_rx) = mpsc::channel();
        thread::spawn(move || preview_worker_loop(preview_request_rx, preview_event_tx));
        let shutdown = Arc::new(Mutex::new(None));
        let control_slot = Arc::new(Mutex::new(None));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_control = Arc::clone(&control_slot);
        thread::spawn(move || worker_loop(command_rx, event_tx, thread_shutdown, thread_control));
        let initial_preferences = UserPreferences::from_config(&config);
        let mut app = Self {
            config,
            status: "Starting server".to_owned(),
            command_tx,
            event_rx,
            control_slot,
            preview_request_tx,
            preview_event_rx,
            preview_pending: 0,
            source_tab,
            // The config's default source is only a placeholder.  This native
            // UI requires an explicit source-card (or test-pattern) choice.
            source_selected: false,
            sources: Vec::new(),
            source_previews: HashMap::new(),
            test_preview: None,
            last_source_refresh: None,
            last_window_preview_refresh: None,
            source_error: None,
            last_saved_preferences: Some(initial_preferences),
        };
        // Queue both the visible source type and the window thumbnails on the
        // background worker.  Window capture can be slower than monitor
        // capture, so this removes the empty wait when a user later opens the
        // Windows tab without blocking the native UI thread.
        app.request_source_refresh();
        if app.source_tab != SourceTab::Windows {
            app.request_window_preview_refresh();
        }
        // Bring the control/viewer server online with a neutral local source.
        // Persisted HWNDs are intentionally not trusted before the user makes
        // the explicit selection required by this UI.
        let _ = app
            .command_tx
            .send(UiCommand::StartServer(Box::new(server_bootstrap_config(
                &app.config,
            ))));
        let _ = app.command_tx.send(UiCommand::LookupPublicIp);
        app
    }

    fn poll_preview_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.preview_event_rx.try_recv() {
            self.preview_pending = self.preview_pending.saturating_sub(1);
            match event {
                PreviewEvent::Updated {
                    mut sources,
                    previews,
                } => {
                    self.source_error = None;
                    let selected_native_exists = self
                        .config
                        .source
                        .native_id
                        .is_some_and(capture::native_window_exists);
                    retain_selected_native_source(
                        &mut sources,
                        &self.sources,
                        self.source_selected,
                        &self.config.source.kind,
                        self.config.source.native_id,
                        selected_native_exists,
                    );
                    if self.source_selected
                        && self.config.source.kind == "window"
                        && let Some(native_id) = self.config.source.native_id
                        && let Some(source) = sources
                            .iter()
                            .find(|source| source.native_id == Some(native_id))
                    {
                        // The enumeration order changes as windows open, close,
                        // minimize, and regain focus. Keep the index only as a
                        // display/legacy hint; the native HWND is authoritative.
                        self.config.source.index = source.index;
                    }
                    self.sources = sources;
                    for frame in previews {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [frame.preview.width, frame.preview.height],
                            &frame.preview.rgba,
                        );
                        let texture = ctx.load_texture(
                            frame.key.clone(),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.source_previews.insert(frame.key, texture);
                    }
                }
                PreviewEvent::TestPattern(frame) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.preview.width, frame.preview.height],
                        &frame.preview.rgba,
                    );
                    self.test_preview =
                        Some(ctx.load_texture(frame.key, image, egui::TextureOptions::LINEAR));
                }
                PreviewEvent::Failed(error) => {
                    self.source_error = Some(error);
                    self.sources.clear();
                }
            }
        }
    }

    fn request_source_refresh(&mut self) {
        let (kind, width, height, fps) = match self.source_tab {
            SourceTab::Displays => ("monitor", 0, 0, 0),
            SourceTab::Windows => ("window", 0, 0, 0),
            SourceTab::TestPattern => {
                let settings = CaptureSettings::from_config(&self.config);
                let (width, height) = settings.test_pattern_dimensions();
                (
                    "test",
                    width,
                    height,
                    settings.output_fps.unwrap_or(settings.fps),
                )
            }
        };
        if self.enqueue_preview_request(PreviewRequest {
            kind,
            width,
            height,
            fps,
        }) {
            self.last_source_refresh = Some(Instant::now());
            if kind == "window" {
                self.last_window_preview_refresh = self.last_source_refresh;
            }
        }
    }

    fn request_window_preview_refresh(&mut self) {
        if self.enqueue_preview_request(PreviewRequest {
            kind: "window",
            width: 0,
            height: 0,
            fps: 0,
        }) {
            self.last_window_preview_refresh = Some(Instant::now());
        }
    }

    fn enqueue_preview_request(&mut self, request: PreviewRequest) -> bool {
        if self.preview_request_tx.send(request).is_err() {
            return false;
        }
        self.preview_pending = self.preview_pending.saturating_add(1);
        true
    }

    fn has_valid_source_selection(&self) -> bool {
        source_selection_is_valid(
            self.source_selected,
            &self.config.source.kind,
            self.config.source.index,
            self.config.source.native_id,
            &self.sources,
        )
    }

    fn selected_window_is_minimized(&self) -> bool {
        self.config.source.kind == "window"
            && self
                .config
                .source
                .native_id
                .is_some_and(capture::native_window_is_minimized)
    }

    fn draw_source_picker(&mut self, ui: &mut egui::Ui) {
        self.poll_preview_events(ui.ctx());
        ui.heading("Source");
        ui.horizontal(|ui| {
            let displays_response = ui
                .selectable_label(self.source_tab == SourceTab::Displays, "Displays")
                .on_hover_text("Choose a monitor to capture.");
            if displays_response.clicked() {
                if self.source_tab != SourceTab::Displays {
                    self.source_selected = false;
                }
                self.source_tab = SourceTab::Displays;
                self.last_source_refresh = None;
                self.leave_test_pattern("monitor");
            }
            let windows_response = ui
                .selectable_label(self.source_tab == SourceTab::Windows, "Windows")
                .on_hover_text("Choose an application window to capture.");
            if windows_response.clicked() {
                if self.source_tab != SourceTab::Windows {
                    self.source_selected = false;
                }
                self.source_tab = SourceTab::Windows;
                self.last_source_refresh = None;
                self.leave_test_pattern("window");
            }
            let test_response = ui
                .selectable_label(self.source_tab == SourceTab::TestPattern, "Test pattern")
                .on_hover_text("Use a deterministic animated test source.");
            if test_response.clicked() {
                self.source_tab = SourceTab::TestPattern;
                self.config.source.kind = "test".to_owned();
                self.config.source.index = 0;
                self.config.source.native_id = None;
                self.source_selected = true;
                self.config.audio_mode = "off".to_owned();
                if self.config.quality == "source" {
                    self.config.quality = "1080p".to_owned();
                }
                if self.config.fps_preset == "source" {
                    self.config.fps_preset = "60".to_owned();
                }
                if self.config.adaptive_quality_ceiling == "source" {
                    self.config.adaptive_quality_ceiling = "1080p".to_owned();
                }
                if self.config.adaptive_fps_ceiling == "source" {
                    self.config.adaptive_fps_ceiling = "60".to_owned();
                }
            }
        });

        if self.source_tab == SourceTab::TestPattern {
            if self
                .last_source_refresh
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
                && self.preview_pending == 0
            {
                self.request_source_refresh();
            }
            self.draw_test_pattern_preview(ui);
            if let Some(error) = &self.source_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Source error: {error}"),
                );
            }
            return;
        }

        if !self.source_selected {
            ui.label("Select a source card to enable Start Stream.");
        } else if self.selected_window_is_minimized() {
            ui.label("Restore the selected window before starting its capture.");
        }

        if self
            .last_source_refresh
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
            && self.preview_pending == 0
        {
            self.request_source_refresh();
        }

        if should_prefetch_window_previews(
            self.source_tab,
            self.last_window_preview_refresh,
            self.preview_pending,
        ) {
            self.request_window_preview_refresh();
        }

        let kind = match self.source_tab {
            SourceTab::Displays => "monitor",
            SourceTab::Windows => "window",
            SourceTab::TestPattern => return,
        };
        let sources = self
            .sources
            .iter()
            .filter(|source| source.kind == kind)
            .cloned()
            .collect::<Vec<_>>();
        if sources.is_empty() {
            ui.add_space(12.0);
            ui.label(if kind == "monitor" {
                "No displays were found."
            } else {
                "No capturable windows were found."
            });
        } else {
            let carousel_rect =
                egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 190.0));
            let pointer_over_carousel = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|position| carousel_rect.contains(position))
            });
            if pointer_over_carousel {
                ui.input_mut(|input| {
                    if input.smooth_scroll_delta.x.abs() < f32::EPSILON
                        && input.smooth_scroll_delta.y.abs() >= f32::EPSILON
                    {
                        input.smooth_scroll_delta.x = input.smooth_scroll_delta.y;
                        input.smooth_scroll_delta.y = 0.0;
                    }
                });
            }
            egui::ScrollArea::horizontal()
                .id_salt("source-carousel")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for source in sources {
                            let key = source_key(&source);
                            let selected = self.source_selected
                                && source_matches_selection(
                                    &source,
                                    &self.config.source.kind,
                                    self.config.source.index,
                                    self.config.source.native_id,
                                );
                            let texture = self.source_previews.get(&key).cloned();
                            let source_kind = source.kind.clone();
                            let source_index = source.index;
                            let source_native_id = source.native_id;
                            let frame_response = egui::Frame::new()
                                .fill(ui.visuals().faint_bg_color)
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    if selected {
                                        ui.visuals().selection.bg_fill
                                    } else {
                                        ui.visuals().widgets.noninteractive.bg_stroke.color
                                    },
                                ))
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(300.0);
                                        ui.add_sized(
                                            [280.0, 0.0],
                                            egui::Label::new(
                                                egui::RichText::new(&source.name).strong(),
                                            )
                                            .wrap(),
                                        );
                                        ui.add_space(6.0);
                                        let tile_size = egui::vec2(280.0, 158.0);
                                        ui.allocate_ui_with_layout(
                                            tile_size,
                                            egui::Layout::centered_and_justified(
                                                egui::Direction::LeftToRight,
                                            ),
                                            |ui| {
                                                if let Some(texture) = texture {
                                                    ui.add(
                                                        egui::Image::from_texture(&texture)
                                                            .fit_to_exact_size(tile_size)
                                                            .maintain_aspect_ratio(true),
                                                    );
                                                } else {
                                                    ui.label("Preview unavailable");
                                                }
                                            },
                                        );
                                        ui.add_space(6.0);
                                        ui.label(format!(
                                            "{} × {}{}",
                                            source.width,
                                            source.height,
                                            source
                                                .fps
                                                .map(|fps| format!(" · {fps} FPS"))
                                                .unwrap_or_default()
                                        ));
                                    });
                                });
                            let card_response = ui.interact(
                                frame_response.response.rect,
                                ui.id().with(("source-card", key)),
                                egui::Sense::click(),
                            );
                            if card_response.clicked() {
                                self.config.source.kind = source_kind;
                                self.config.source.index = source_index;
                                self.config.source.native_id = source_native_id;
                                self.source_selected = true;
                                if self.config.source.kind == "monitor"
                                    && self.config.audio_mode == "window"
                                {
                                    self.config.audio_mode = "system".to_owned();
                                } else if self.config.source.kind == "window"
                                    && self.config.audio_mode == "system"
                                {
                                    self.config.audio_mode = "window".to_owned();
                                }
                            }
                            ui.add_space(8.0);
                        }
                    });
                });
        }
        if let Some(error) = &self.source_error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Source error: {error}"),
            );
        }
    }

    fn leave_test_pattern(&mut self, kind: &str) {
        if self.config.source.kind != "test" {
            return;
        }
        if let Some(source) = self.sources.iter().find(|source| source.kind == kind) {
            self.config.source.kind = source.kind.clone();
            self.config.source.index = source.index;
            self.config.source.native_id = source.native_id;
        } else {
            self.config.source.kind = kind.to_owned();
            self.config.source.index = 0;
            self.config.source.native_id = None;
        }
    }

    fn available_audio_processes(&self) -> Vec<String> {
        let mut processes = HashSet::new();
        for source in &self.sources {
            if source.kind != "window" {
                continue;
            }
            let process = source
                .name
                .split_once(':')
                .map(|(name, _)| name.trim())
                .unwrap_or(source.name.as_str())
                .to_owned();
            if !process.is_empty() {
                processes.insert(process);
            }
        }
        let mut processes = processes.into_iter().collect::<Vec<_>>();
        processes.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        processes
    }

    fn draw_audio_settings(
        &mut self,
        ui: &mut egui::Ui,
        stream_active: bool,
        available_audio_processes: &[String],
    ) {
        let audio_mode = if self.source_tab == SourceTab::Windows {
            "window"
        } else {
            "system"
        };
        let audio_label = if audio_mode == "window" {
            "Capture window audio"
        } else {
            "Capture system audio"
        };
        let mut capture_audio = self.config.audio_mode == audio_mode;
        ui.horizontal(|ui| {
            ui.label("Audio");
            let audio_response = ui
                .add_enabled(
                    !stream_active,
                    egui::Checkbox::new(&mut capture_audio, audio_label),
                )
                .on_hover_text(if stream_active {
                    "Stop the stream before changing audio capture."
                } else {
                    "Enable audio for the selected source."
                });
            if audio_response.changed() {
                self.config.audio_mode = if capture_audio {
                    audio_mode.to_owned()
                } else {
                    "off".to_owned()
                };
            }
        });

        if audio_mode != "system" || self.config.audio_mode != "system" {
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Exclude audio");
            let picker_width = (ui.available_width() - 290.0).max(180.0);
            ui.add_enabled_ui(!stream_active, |ui| {
                egui::ComboBox::from_id_salt("audio-process-picker")
                    .selected_text("Add process to ignore list")
                    .width(picker_width)
                    .show_ui(ui, |ui| {
                        for process in available_audio_processes {
                            if self
                                .config
                                .excluded_audio_processes
                                .iter()
                                .any(|excluded| excluded.eq_ignore_ascii_case(process))
                            {
                                continue;
                            }
                            if ui.selectable_label(false, process).clicked() {
                                self.config.excluded_audio_processes.push(process.clone());
                                ui.close();
                            }
                        }
                    });
            });
            if ui
                .add_enabled(!stream_active, egui::Button::new("Add default ignore list"))
                .on_hover_text("Add currently running matches such as Discord or Telegram.")
                .clicked()
            {
                for default_process in DEFAULT_AUDIO_EXCLUSIONS {
                    if available_audio_processes
                        .iter()
                        .any(|process| process.eq_ignore_ascii_case(default_process))
                        && !self
                            .config
                            .excluded_audio_processes
                            .iter()
                            .any(|process| process.eq_ignore_ascii_case(default_process))
                    {
                        self.config
                            .excluded_audio_processes
                            .push((*default_process).to_owned());
                    }
                }
            }
            if ui
                .add_enabled(!stream_active, egui::Button::new("Clear"))
                .on_hover_text("Remove every process from the ignored-audio list.")
                .clicked()
            {
                self.config.excluded_audio_processes.clear();
            }
        });

        if self.config.excluded_audio_processes.is_empty() {
            return;
        }
        ui.label("Ignored");
        let ignored = self.config.excluded_audio_processes.clone();
        let column_count = ((ui.available_width() / 180.0).floor() as usize).clamp(1, 4);
        egui::Grid::new("ignored-audio-processes")
            .num_columns(column_count)
            .spacing(egui::vec2(12.0, 4.0))
            .show(ui, |ui| {
                for (index, process) in ignored.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add_sized([130.0, 20.0], egui::Label::new(process).truncate())
                            .on_hover_text(process);
                        if ui
                            .add_enabled(!stream_active, egui::Button::new("Remove"))
                            .clicked()
                        {
                            self.config
                                .excluded_audio_processes
                                .retain(|value| value != process);
                        }
                    });
                    if (index + 1) % column_count == 0 {
                        ui.end_row();
                    }
                }
                if !ignored.len().is_multiple_of(column_count) {
                    ui.end_row();
                }
            });
    }

    fn draw_test_pattern_preview(&self, ui: &mut egui::Ui) {
        if let Some(texture) = &self.test_preview {
            ui.add_space(10.0);
            let tile_size = egui::vec2(280.0, 158.0);
            ui.allocate_ui_with_layout(
                tile_size,
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.add(
                        egui::Image::from_texture(texture)
                            .fit_to_exact_size(tile_size)
                            .maintain_aspect_ratio(true),
                    );
                },
            );
            let settings = CaptureSettings::from_config(&self.config);
            let (pattern_width, pattern_height) = settings.test_pattern_dimensions();
            let pattern_fps = settings.output_fps.unwrap_or(settings.fps);
            ui.label(format!(
                "Test pattern · {pattern_width} × {pattern_height} · {pattern_fps} FPS"
            ));
            return;
        }
        ui.add_space(10.0);
        let width = 280.0;
        let height = 158.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 20, 24));

        let colors = [
            egui::Color32::from_rgb(255, 255, 255),
            egui::Color32::from_rgb(255, 255, 0),
            egui::Color32::from_rgb(0, 255, 255),
            egui::Color32::from_rgb(0, 255, 0),
            egui::Color32::from_rgb(255, 0, 255),
            egui::Color32::from_rgb(255, 0, 0),
            egui::Color32::from_rgb(0, 0, 255),
            egui::Color32::from_rgb(0, 0, 0),
        ];
        let bar_width = rect.width() / colors.len() as f32;
        for (index, color) in colors.into_iter().enumerate() {
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.left() + index as f32 * bar_width, rect.top()),
                egui::pos2(
                    rect.left() + (index + 1) as f32 * bar_width,
                    rect.center().y,
                ),
            );
            painter.rect_filled(bar, 0.0, color);
        }

        let lower = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.center().y),
            rect.right_bottom(),
        );
        painter.rect_filled(lower, 0.0, egui::Color32::from_rgb(35, 38, 44));
        for index in 0..8 {
            let shade = (index * 32) as u8;
            let x0 = lower.left() + index as f32 * lower.width() / 8.0;
            let x1 = lower.left() + (index + 1) as f32 * lower.width() / 8.0;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, lower.top()),
                    egui::pos2(x1, lower.bottom()),
                ),
                0.0,
                egui::Color32::from_gray(shade),
            );
        }

        let phase = (ui.input(|input| input.time) as f32 * 0.35).fract();
        let sweep_x = rect.left() + rect.width() * phase;
        painter.line_segment(
            [
                egui::pos2(sweep_x, rect.top()),
                egui::pos2(sweep_x, rect.bottom()),
            ],
            egui::Stroke::new(2.0, egui::Color32::WHITE),
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "TEST PATTERN",
            egui::FontId::proportional(22.0),
            egui::Color32::WHITE,
        );
        let settings = CaptureSettings::from_config(&self.config);
        let (pattern_width, pattern_height) = settings.test_pattern_dimensions();
        let pattern_fps = settings.output_fps.unwrap_or(settings.fps);
        ui.label(format!(
            "Test pattern · {pattern_width} × {pattern_height} · {pattern_fps} FPS"
        ));
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::ServerReady(_url) => {
                    self.status = "Ready".to_owned();
                }
                UiEvent::StreamStarted => {
                    self.status = "Running".to_owned();
                }
                UiEvent::StreamStopped => {
                    self.status = "Ready".to_owned();
                }
                UiEvent::Failed(error) => {
                    self.status = format!("Error: {error}");
                }
                UiEvent::PublicIp(address) => {
                    self.config.advertise_host = Some(address);
                }
                UiEvent::PublicIpFailed(_error) => {}
            }
        }
    }
}

fn source_key(source: &CaptureSourceInfo) -> String {
    match source.native_id {
        Some(native_id) => format!("source-preview-{}-native-{native_id}", source.kind),
        None => format!("source-preview-{}-index-{}", source.kind, source.index),
    }
}

fn server_bootstrap_config(config: &AppConfig) -> AppConfig {
    let mut bootstrap = config.clone();
    bootstrap.source.kind = "test".to_owned();
    bootstrap.source.index = 0;
    bootstrap.source.native_id = None;
    bootstrap.audio_mode = "off".to_owned();
    bootstrap
}

fn should_prefetch_window_previews(
    source_tab: SourceTab,
    last_refresh: Option<Instant>,
    pending_requests: usize,
) -> bool {
    source_tab != SourceTab::Windows
        && pending_requests == 0
        && last_refresh.is_none_or(|last| last.elapsed() >= WINDOW_PREVIEW_REFRESH_INTERVAL)
}

fn source_selection_is_valid(
    source_selected: bool,
    source_kind: &str,
    source_index: usize,
    source_native_id: Option<u64>,
    sources: &[CaptureSourceInfo],
) -> bool {
    source_selected
        && (source_kind == "test"
            || sources.iter().any(|source| {
                source_matches_selection(source, source_kind, source_index, source_native_id)
            }))
}

fn source_matches_selection(
    source: &CaptureSourceInfo,
    source_kind: &str,
    source_index: usize,
    source_native_id: Option<u64>,
) -> bool {
    source.kind == source_kind
        && match source_native_id {
            Some(native_id) => source.native_id == Some(native_id),
            None => source.index == source_index,
        }
}

fn retain_selected_native_source(
    sources: &mut Vec<CaptureSourceInfo>,
    previous_sources: &[CaptureSourceInfo],
    source_selected: bool,
    source_kind: &str,
    source_native_id: Option<u64>,
    native_window_exists: bool,
) {
    let Some(native_id) = source_native_id else {
        return;
    };
    if !source_selected
        || source_kind != "window"
        || !native_window_exists
        || sources
            .iter()
            .any(|source| source.native_id == Some(native_id))
    {
        return;
    }
    if let Some(previous) = previous_sources
        .iter()
        .find(|source| source.native_id == Some(native_id))
    {
        sources.push(previous.clone());
    }
}

fn preview_worker_loop(
    request_rx: mpsc::Receiver<PreviewRequest>,
    event_tx: mpsc::Sender<PreviewEvent>,
) {
    while let Ok(request) = request_rx.recv() {
        if request.kind == "test" {
            let result = crate::packaging::prepare_ffmpeg().and_then(|ffmpeg| {
                capture::capture_test_pattern_preview(
                    &ffmpeg.command,
                    request.width,
                    request.height,
                    request.fps,
                    320,
                    180,
                )
            });
            match result {
                Ok(preview) => {
                    if event_tx
                        .send(PreviewEvent::TestPattern(PreviewFrame {
                            key: "test-pattern-preview".to_owned(),
                            preview,
                        }))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(PreviewEvent::Failed(error.to_string()));
                }
            }
            continue;
        }
        let sources = match capture::list_sources() {
            Ok(sources) => sources,
            Err(error) => {
                let _ = event_tx.send(PreviewEvent::Failed(error.to_string()));
                continue;
            }
        };
        let previews = sources
            .iter()
            .filter(|source| source.kind == request.kind)
            .filter_map(|source| {
                capture::capture_preview(source, 320, 180)
                    .ok()
                    .map(|preview| PreviewFrame {
                        key: source_key(source),
                        preview,
                    })
            })
            .collect();
        if event_tx
            .send(PreviewEvent::Updated { sources, previews })
            .is_err()
        {
            break;
        }
    }
}

fn quality_label(value: &str) -> String {
    match value {
        "source" => "Source".to_owned(),
        "720p" | "1080p" | "1440p" => format!("{value} HD"),
        "2160p" => "2160p 4K".to_owned(),
        "4320p" => "4320p 8K".to_owned(),
        other => other.to_owned(),
    }
}

fn selected_quality_label(value: &str, source: Option<(u32, u32)>) -> String {
    if value == "source"
        && let Some((width, height)) = source
    {
        return format!("Source · {width} × {height}");
    }
    quality_label(value)
}

fn quality_option_label(value: &str, source: Option<(u32, u32)>) -> String {
    if value == "source" {
        selected_quality_label(value, source)
    } else {
        quality_label(value)
    }
}

fn fps_label(value: &str) -> String {
    if value == "source" {
        "Source".to_owned()
    } else {
        format!("{value} FPS")
    }
}

fn selected_fps_label(value: &str, source_fps: Option<u32>) -> String {
    if value == "source"
        && let Some(fps) = source_fps
    {
        return format!("Source · {fps} FPS");
    }
    fps_label(value)
}

fn fps_option_label(value: &str, source_fps: Option<u32>) -> String {
    if value == "source" {
        selected_fps_label(value, source_fps)
    } else {
        fps_label(value)
    }
}

fn quality_mode_label(value: &str) -> &'static str {
    if value == "adaptive" {
        "Auto"
    } else {
        "Manual"
    }
}

impl eframe::App for HostUi {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let previous_settings = CaptureSettings::from_config(&self.config);
        let source_details = if self.source_tab == SourceTab::TestPattern {
            let settings = CaptureSettings::from_config(&self.config);
            let (width, height) = settings.test_pattern_dimensions();
            Some((width, height, settings.output_fps.or(Some(settings.fps))))
        } else {
            self.sources
                .iter()
                .find(|source| {
                    source_matches_selection(
                        source,
                        &self.config.source.kind,
                        self.config.source.index,
                        self.config.source.native_id,
                    )
                })
                .map(|source| (source.width, source.height, source.fps))
        };
        let source_height_limit = (self.source_tab != SourceTab::TestPattern)
            .then(|| source_details.map(|(_, height, _)| height))
            .flatten();
        let source_fps_limit = (self.source_tab != SourceTab::TestPattern)
            .then(|| source_details.and_then(|(_, _, fps)| fps))
            .flatten();
        let available_audio_processes = self.available_audio_processes();
        self.poll_events();
        let stream_active = matches!(
            self.status.as_str(),
            "Starting stream" | "Running" | "Stopping stream"
        );
        if self.source_tab == SourceTab::TestPattern {
            ui.ctx().request_repaint();
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Instant Local Stream");
            ui.separator();

            self.draw_source_picker(ui);
            ui.separator();

            ui.heading("Video");
            egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                ui.label("Cursor");
                ui.checkbox(&mut self.config.draw_mouse, "Capture pointer");
                ui.end_row();

                ui.label("Quality mode");
                egui::ComboBox::from_id_salt("quality-mode")
                    .selected_text(quality_mode_label(&self.config.quality_mode))
                    .show_ui(ui, |ui| {
                        for (value, label) in [("manual", "Manual"), ("adaptive", "Auto")] {
                            ui.selectable_value(
                                &mut self.config.quality_mode,
                                value.to_owned(),
                                label,
                            );
                        }
                    });
                ui.end_row();

                ui.label("Bitrate mode");
                egui::ComboBox::from_id_salt("bitrate-mode")
                    .selected_text(&self.config.bitrate_mode)
                    .show_ui(ui, |ui| {
                        for value in ["fixed", "automatic"] {
                            ui.selectable_value(
                                &mut self.config.bitrate_mode,
                                value.to_owned(),
                                value,
                            );
                        }
                    });
                ui.end_row();

                if self.config.quality_mode == "adaptive" {
                    ui.label("Transcode Groups");
                    egui::ComboBox::from_id_salt("quality-groups")
                        .selected_text(&self.config.max_quality_groups)
                        .show_ui(ui, |ui| {
                            for groups in 1..=4 {
                                let value = groups.to_string();
                                ui.selectable_value(
                                    &mut self.config.max_quality_groups,
                                    value.clone(),
                                    value,
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("Latency preference");
                    egui::ComboBox::from_id_salt("latency-preference")
                        .selected_text(&self.config.latency_preference)
                        .show_ui(ui, |ui| {
                            for value in ["low", "balanced", "quality"] {
                                ui.selectable_value(
                                    &mut self.config.latency_preference,
                                    value.to_owned(),
                                    value,
                                );
                            }
                        });
                    ui.end_row();
                }

                ui.label("Codec");
                egui::ComboBox::from_id_salt("codec")
                    .selected_text(self.config.codec.to_ascii_uppercase())
                    .show_ui(ui, |ui| {
                        for value in ["auto", "vp8", "vp9", "h264"] {
                            ui.selectable_value(
                                &mut self.config.codec,
                                value.to_owned(),
                                if value == "auto" {
                                    "Automatic".to_owned()
                                } else if value == "h264" {
                                    "H.264".to_owned()
                                } else {
                                    value.to_ascii_uppercase()
                                },
                            );
                        }
                    });
                ui.end_row();

                let adaptive_mode = self.config.quality_mode == "adaptive";
                let mut selected_quality = if adaptive_mode {
                    self.config.adaptive_quality_ceiling.clone()
                } else {
                    self.config.quality.clone()
                };
                ui.label(if adaptive_mode {
                    "Quality ceiling"
                } else {
                    "Resolution"
                });
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(selected_quality_label(
                        &selected_quality,
                        source_details.map(|(width, height, _)| (width, height)),
                    ))
                    .show_ui(ui, |ui| {
                        for value in QUALITY_PRESETS {
                            if self.source_tab == SourceTab::TestPattern && *value == "source" {
                                continue;
                            }
                            if source_height_limit.is_some_and(|limit| {
                                quality_height(value).is_some_and(|height| height > limit)
                            }) {
                                continue;
                            }
                            ui.selectable_value(
                                &mut selected_quality,
                                (*value).to_owned(),
                                quality_option_label(
                                    value,
                                    source_details.map(|(width, height, _)| (width, height)),
                                ),
                            );
                        }
                    });
                if adaptive_mode {
                    self.config.adaptive_quality_ceiling = selected_quality;
                } else {
                    self.config.quality = selected_quality;
                }
                ui.end_row();

                let mut selected_fps = if adaptive_mode {
                    self.config.adaptive_fps_ceiling.clone()
                } else {
                    self.config.fps_preset.clone()
                };
                ui.label(if adaptive_mode {
                    "FPS ceiling"
                } else {
                    "Frame rate"
                });
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(selected_fps_label(
                        &selected_fps,
                        source_details.and_then(|(_, _, fps)| fps),
                    ))
                    .show_ui(ui, |ui| {
                        for value in FPS_PRESETS {
                            if self.source_tab == SourceTab::TestPattern && *value == "source" {
                                continue;
                            }
                            if source_fps_limit.is_some_and(|limit| {
                                fps_value(value).is_some_and(|fps| fps > limit)
                            }) {
                                continue;
                            }
                            ui.selectable_value(
                                &mut selected_fps,
                                (*value).to_owned(),
                                fps_option_label(value, source_details.and_then(|(_, _, fps)| fps)),
                            );
                        }
                    });
                if adaptive_mode {
                    self.config.adaptive_fps_ceiling = selected_fps;
                } else {
                    self.config.fps_preset = selected_fps;
                }
                ui.end_row();

                ui.label("Bitrate");
                if self.config.bitrate_mode == "fixed" {
                    ui.add(
                        egui::DragValue::new(&mut self.config.bitrate).range(250_000..=50_000_000),
                    );
                } else {
                    ui.label(format!(
                        "Automatic · {} bps",
                        self.config.effective_bitrate()
                    ));
                }
                ui.end_row();
            });

            ui.separator();
            ui.heading("Audio");
            if self.source_tab != SourceTab::TestPattern {
                self.draw_audio_settings(ui, stream_active, &available_audio_processes);
            } else {
                ui.label("Audio is unavailable for the test pattern.");
            }
            ui.separator();
            ui.heading("Stream");
            ui.horizontal(|ui| {
                let stream_running = matches!(
                    self.status.as_str(),
                    "Starting stream" | "Running" | "Stopping stream"
                );
                let server_ready = self.status == "Ready";
                let source_selected = self.has_valid_source_selection();
                let source_minimized = self.selected_window_is_minimized();
                let source_ready = source_selected && !source_minimized;
                let stream_button = if stream_running {
                    ui.add_enabled(
                        self.status != "Stopping stream",
                        egui::Button::new("Stop Stream"),
                    )
                    .on_hover_text("Stop media delivery while keeping the viewer server online.")
                } else {
                    ui.add_enabled(
                        server_ready && source_ready,
                        egui::Button::new("Start Stream"),
                    )
                    .on_hover_text(if !source_selected {
                        "Select a monitor, window, or test-pattern source before starting."
                    } else if source_minimized {
                        "Restore the selected window before starting its capture."
                    } else if server_ready {
                        "Start media delivery for connected viewers."
                    } else {
                        "Waiting for the viewer server to start."
                    })
                };
                if stream_button.clicked() {
                    if stream_running {
                        let _ = self.command_tx.send(UiCommand::StopStream);
                        self.status = "Stopping stream".to_owned();
                    } else {
                        match self.config.validate() {
                            Ok(()) => {
                                let _ = self.command_tx.send(UiCommand::StartStream(Box::new(
                                    CaptureSettings::from_config(&self.config),
                                )));
                                self.status = "Starting stream".to_owned();
                            }
                            Err(error) => self.status = format!("Error: {error}"),
                        }
                    }
                }
                ui.label(format!("Status: {}", self.status));
            });
            let mut viewer_url = self.config.viewer_url();
            ui.horizontal(|ui| {
                ui.label("Viewer URL");
                ui.add(egui::TextEdit::singleline(&mut viewer_url).desired_width(420.0));
                if ui
                    .button("Copy")
                    .on_hover_text("Copy the current viewer URL.")
                    .clicked()
                {
                    ui.ctx().copy_text(viewer_url.clone());
                }
                let refresh_tooltip = if stream_active {
                    "Stop the stream before refreshing the token."
                } else {
                    "Generate a new token and viewer URL."
                };
                let refresh_response = ui
                    .add_enabled(!stream_active, egui::Button::new("Refresh token"))
                    .on_hover_text(refresh_tooltip);
                if refresh_response.clicked() {
                    self.config.token = generate_token();
                }
            });
            ui.horizontal(|ui| {
                ui.label("HTTP port");
                ui.add(egui::DragValue::new(&mut self.config.http_port).range(1..=65535));
            });
        });

        let preferences_snapshot = UserPreferences::from_config(&self.config);
        if self.last_saved_preferences.as_ref() != Some(&preferences_snapshot) {
            if let Err(error) = preferences::save(&self.config) {
                tracing::debug!(%error, "could not persist UI preferences");
            }
            self.last_saved_preferences = Some(preferences_snapshot);
        }

        if self.status == "Running" {
            let current_settings = CaptureSettings::from_config(&self.config);
            if current_settings != previous_settings
                && let Ok(slot) = self.control_slot.lock()
                && let Some(sender) = slot.as_ref()
            {
                let _ = sender.send(server::ServerCommand::Update(current_settings));
            }
        }
    }

    fn on_exit(&mut self) {
        let _ = self.command_tx.send(UiCommand::Shutdown);
    }
}

fn worker_loop(
    command_rx: mpsc::Receiver<UiCommand>,
    event_tx: mpsc::Sender<UiEvent>,
    shutdown_slot: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    control_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<server::ServerCommand>>>>,
) {
    while let Ok(command) = command_rx.recv() {
        match command {
            UiCommand::StartServer(config) => {
                let config = *config;
                if shutdown_slot
                    .lock()
                    .map(|slot| slot.is_some())
                    .unwrap_or(true)
                {
                    let _ = event_tx.send(UiEvent::Failed("server is already running".to_owned()));
                    continue;
                }
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
                if let Ok(mut slot) = shutdown_slot.lock() {
                    *slot = Some(shutdown_tx);
                }
                if let Ok(mut slot) = control_slot.lock() {
                    *slot = Some(control_tx);
                }
                let event_tx = event_tx.clone();
                let thread_shutdown = Arc::clone(&shutdown_slot);
                let thread_control = Arc::clone(&control_slot);
                let url = config.viewer_url();
                let _ = event_tx.send(UiEvent::ServerReady(url));
                thread::spawn(move || {
                    let result = tokio::runtime::Runtime::new()
                        .context("create Tokio runtime")
                        .and_then(|runtime| {
                            runtime.block_on(server::run_with_control(
                                config,
                                shutdown_rx,
                                control_rx,
                            ))
                        });
                    if let Ok(mut slot) = thread_shutdown.lock() {
                        *slot = None;
                    }
                    if let Ok(mut slot) = thread_control.lock() {
                        *slot = None;
                    }
                    match result {
                        Ok(()) => {
                            let _ = event_tx.send(UiEvent::StreamStopped);
                        }
                        Err(error) => {
                            let _ =
                                event_tx.send(UiEvent::Failed(format!("server failed: {error}")));
                        }
                    }
                });
            }
            UiCommand::StartStream(settings) => {
                let settings = *settings;
                if let Ok(slot) = control_slot.lock()
                    && let Some(sender) = slot.as_ref()
                {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    if sender
                        .send(server::ServerCommand::StartStream {
                            settings,
                            result: Some(result_tx),
                        })
                        .is_err()
                    {
                        let _ = event_tx.send(UiEvent::Failed(
                            "viewer server stopped unexpectedly".to_owned(),
                        ));
                        continue;
                    }
                    match result_rx.blocking_recv() {
                        Ok(Ok(())) => {
                            let _ = event_tx.send(UiEvent::StreamStarted);
                        }
                        Ok(Err(error)) => {
                            let _ = event_tx
                                .send(UiEvent::Failed(format!("could not start stream: {error}")));
                        }
                        Err(_) => {
                            let _ = event_tx.send(UiEvent::Failed(
                                "viewer server stopped before starting the stream".to_owned(),
                            ));
                        }
                    }
                } else {
                    let _ =
                        event_tx.send(UiEvent::Failed("viewer server is not ready yet".to_owned()));
                }
            }
            UiCommand::StopStream => {
                if let Ok(slot) = control_slot.lock()
                    && let Some(sender) = slot.as_ref()
                {
                    let _ = sender.send(server::ServerCommand::StopStream);
                    let _ = event_tx.send(UiEvent::StreamStopped);
                }
            }
            UiCommand::Shutdown => {
                if let Ok(mut slot) = shutdown_slot.lock()
                    && let Some(sender) = slot.take()
                {
                    let _ = sender.send(());
                }
                if let Ok(mut slot) = control_slot.lock() {
                    slot.take();
                }
            }
            UiCommand::LookupPublicIp => match crate::network::lookup_public_ipv4() {
                Ok(address) => {
                    let _ = event_tx.send(UiEvent::PublicIp(address.to_string()));
                }
                Err(error) => {
                    let _ = event_tx.send(UiEvent::PublicIpFailed(error.to_string()));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_preview_prefetch_runs_off_the_windows_tab_only_when_idle() {
        assert!(should_prefetch_window_previews(
            SourceTab::Displays,
            None,
            0
        ));
        assert!(!should_prefetch_window_previews(
            SourceTab::Windows,
            None,
            0
        ));
        assert!(!should_prefetch_window_previews(
            SourceTab::Displays,
            None,
            1
        ));
        assert!(!should_prefetch_window_previews(
            SourceTab::Displays,
            Some(Instant::now()),
            0
        ));
        assert!(should_prefetch_window_previews(
            SourceTab::Displays,
            Some(Instant::now() - WINDOW_PREVIEW_REFRESH_INTERVAL),
            0
        ));
    }

    #[test]
    fn stream_start_requires_an_explicit_valid_source_selection() {
        let monitor = CaptureSourceInfo {
            kind: "monitor".to_owned(),
            index: 0,
            native_id: None,
            width: 1_920,
            height: 1_080,
            fps: Some(60),
            pid: None,
            name: "Display".to_owned(),
        };
        assert!(!source_selection_is_valid(
            false,
            "monitor",
            0,
            None,
            &[monitor.clone()]
        ));
        assert!(!source_selection_is_valid(
            true,
            "monitor",
            1,
            None,
            &[monitor.clone()]
        ));
        assert!(source_selection_is_valid(
            true,
            "monitor",
            0,
            None,
            &[monitor]
        ));
        assert!(source_selection_is_valid(true, "test", 0, None, &[]));
    }

    #[test]
    fn window_selection_uses_native_identity_when_enumeration_order_changes() {
        let selected = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 7,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Selected".to_owned(),
        };
        let unrelated = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 2,
            native_id: Some(99),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(200),
            name: "Unrelated".to_owned(),
        };

        assert!(source_selection_is_valid(
            true,
            "window",
            unrelated.index,
            selected.native_id,
            &[unrelated, selected]
        ));
    }

    #[test]
    fn minimized_selected_window_is_retained_when_enumeration_hides_it() {
        let selected = CaptureSourceInfo {
            kind: "window".to_owned(),
            index: 3,
            native_id: Some(42),
            width: 800,
            height: 600,
            fps: Some(60),
            pid: Some(100),
            name: "Selected".to_owned(),
        };
        let mut refreshed = Vec::new();

        retain_selected_native_source(&mut refreshed, &[selected], true, "window", Some(42), true);

        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].native_id, Some(42));
    }

    #[test]
    fn ui_server_bootstrap_never_trusts_a_persisted_window_handle() {
        let mut config = AppConfig::default();
        config.source.kind = "window".to_owned();
        config.source.index = 7;
        config.source.native_id = Some(42);
        config.audio_mode = "window".to_owned();

        let bootstrap = server_bootstrap_config(&config);

        assert_eq!(bootstrap.source.kind, "test");
        assert_eq!(bootstrap.source.native_id, None);
        assert_eq!(bootstrap.audio_mode, "off");
        assert_eq!(bootstrap.http_port, config.http_port);
        assert_eq!(bootstrap.token, config.token);
    }
}
