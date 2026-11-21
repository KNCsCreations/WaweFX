use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use qrcode::{Color as QrColor, QrCode};

use crate::audio::yt_downloader::{self, YoutubeDownloadEvent};
use crate::audio::{AudioDeviceList, AudioEngine, AudioEngineStatus, PREVIEW_SOUND_ID, apo_installer};
use crate::config::{
    AppConfig, MAX_PITCH_STEP, MIN_PITCH_STEP, MIN_TRIM_GAP, ShortcutOwner, SoundItem, ThemeConfig,
    ThemePreset, rgb_to_color32, sanitize_trim_range,
};
use crate::hotkeys::{self, HotkeyStatus, ShortcutCapture};
use crate::updater::{self, CURRENT_VERSION, GITHUB_REPO_SLUG, UpdateCheckState};

fn is_light_rgb(rgb: [u8; 3]) -> bool {
    let luma = 0.299 * rgb[0] as f32 + 0.587 * rgb[1] as f32 + 0.114 * rgb[2] as f32;
    luma > 128.0
}

fn contrast_on_accent(accent: Color32) -> Color32 {
    let luma = 0.299 * accent.r() as f32 + 0.587 * accent.g() as f32 + 0.114 * accent.b() as f32;
    if luma > 175.0 {
        Color32::from_rgb(0x2B, 0x25, 0x20)
    } else {
        Color32::WHITE
    }
}

/// Applies the active [`ThemeConfig`] palette across all `egui` windows, popups, dropdowns, and widgets.
pub fn apply_theme_visuals(ctx: &egui::Context, theme: &ThemeConfig) {
    let bg_color = rgb_to_color32(theme.bg_main);
    let surface_color = rgb_to_color32(theme.bg_toolbar);
    let card_color = rgb_to_color32(theme.pad_card_bg);
    let hover_color = rgb_to_color32(theme.pad_card_hover);
    let border_color = rgb_to_color32(theme.toolbar_divider);
    let accent = rgb_to_color32(theme.accent_color);
    let text_color = rgb_to_color32(theme.pad_text);
    let muted_color = rgb_to_color32(theme.icon_secondary);
    let light_mode = is_light_rgb(theme.bg_toolbar);

    let mut visuals = if light_mode {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };

    visuals.panel_fill = bg_color;
    visuals.window_fill = surface_color;
    visuals.window_stroke = Stroke::new(1.0_f32, border_color);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.faint_bg_color = card_color;
    visuals.extreme_bg_color = bg_color;
    visuals.code_bg_color = card_color;
    visuals.override_text_color = Some(text_color);

    let accent_fg = contrast_on_accent(accent);
    visuals.selection.bg_fill = accent;
    visuals.selection.stroke = Stroke::new(1.0_f32, accent_fg);

    let r = CornerRadius::same(7);
    visuals.widgets.noninteractive.bg_fill = surface_color;
    visuals.widgets.noninteractive.weak_bg_fill = surface_color;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border_color);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, muted_color);
    visuals.widgets.noninteractive.corner_radius = r;

    visuals.widgets.inactive.bg_fill = card_color;
    visuals.widgets.inactive.weak_bg_fill = card_color;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border_color);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text_color);
    visuals.widgets.inactive.corner_radius = r;

    visuals.widgets.hovered.bg_fill = hover_color;
    visuals.widgets.hovered.weak_bg_fill = hover_color;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.2_f32, accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text_color);
    visuals.widgets.hovered.corner_radius = r;

    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.active.weak_bg_fill = accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.2_f32, accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, accent_fg);
    visuals.widgets.active.corner_radius = r;

    visuals.widgets.open.bg_fill = hover_color;
    visuals.widgets.open.weak_bg_fill = hover_color;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, text_color);
    visuals.widgets.open.corner_radius = r;

    ctx.set_visuals(visuals);
}

fn themed_modal_frame(ctx: &egui::Context, theme: &ThemeConfig) -> egui::Frame {
    egui::Frame::window(&ctx.style())
        .fill(rgb_to_color32(theme.bg_toolbar))
        .stroke(Stroke::new(1.0_f32, rgb_to_color32(theme.toolbar_divider)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(16))
}

fn primary_accent_button<'a>(label: &'a str, theme: &ThemeConfig) -> egui::Button<'a> {
    let accent = rgb_to_color32(theme.accent_color);
    let fg = contrast_on_accent(accent);
    egui::Button::new(egui::RichText::new(label).strong().color(fg))
        .fill(accent)
        .stroke(Stroke::new(1.0_f32, accent))
        .corner_radius(CornerRadius::same(7))
}

#[cfg(target_os = "windows")]
const VBCABLE_ZIP_URL: &str =
    "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip";
#[cfg(target_os = "windows")]
const VBCABLE_WEB_URL: &str = "https://vb-audio.com/Cable/";
#[cfg(target_os = "macos")]
const BLACKHOLE_WEB_URL: &str = "https://existential.audio/blackhole/";

/// Live background progress for the one-click Virtual Audio Driver setup wizard.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DriverInstallPhase {
    #[default]
    Idle,
    Running(String),
    Succeeded(String),
    Failed(String),
}

/// Shared non-blocking controller for downloading/launching virtual audio driver installers.
#[derive(Clone, Default)]
pub struct DriverSetupState {
    phase: Arc<Mutex<DriverInstallPhase>>,
}

impl DriverSetupState {
    pub fn phase(&self) -> DriverInstallPhase {
        self.phase
            .lock()
            .map(|g| g.clone())
            .unwrap_or(DriverInstallPhase::Idle)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase(), DriverInstallPhase::Running(_))
    }

    fn set_phase(phase: &Arc<Mutex<DriverInstallPhase>>, next: DriverInstallPhase) {
        if let Ok(mut guard) = phase.lock() {
            *guard = next;
        }
    }

    /// Launches the platform virtual audio driver installation in a background thread.
    pub fn start_driver_install(&self) {
        if self.is_running() {
            return;
        }
        let phase = Arc::clone(&self.phase);
        Self::set_phase(
            &phase,
            DriverInstallPhase::Running("Starting virtual audio driver installer...".to_string()),
        );
        std::thread::spawn(move || {
            run_platform_driver_install(&phase);
        });
    }

    /// Opens the official virtual audio driver webpage in the user's browser.
    pub fn open_driver_website(&self) {
        if let Err(err) = open_platform_driver_website() {
            Self::set_phase(
                &self.phase,
                DriverInstallPhase::Failed(format!("Could not open browser: {err}")),
            );
        }
    }
}

/// Returns `true` if `name` matches a supported virtual audio driver on Windows or macOS.
pub fn is_known_virtual_audio_driver(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("cable input")
        || lower.contains("vb-audio")
        || lower.contains("voicemeeter")
        || lower.contains("blackhole")
        || lower.contains("loopback")
}

/// Returns `true` if any recognized virtual audio output device exists in `devices`.
pub fn has_virtual_audio_device(devices: &AudioDeviceList) -> bool {
    devices
        .output_devices
        .iter()
        .any(|name| is_known_virtual_audio_driver(name))
}

/// Selects the best detected virtual audio output device (`CABLE Input` / `BlackHole 2ch` preferred).
pub fn find_preferred_virtual_mic_output(devices: &AudioDeviceList) -> Option<String> {
    let mut candidates: Vec<&String> = devices
        .output_devices
        .iter()
        .filter(|name| is_known_virtual_audio_driver(name))
        .collect();
    candidates.sort_by(|a, b| {
        preferred_auto_select_rank(a)
            .cmp(&preferred_auto_select_rank(b))
            .then_with(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))
    });
    candidates.first().map(|s| (*s).clone())
}

fn preferred_auto_select_rank(name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("cable input") || lower.contains("blackhole 2ch") {
        0
    } else if lower.contains("blackhole") || lower.contains("vb-audio") {
        1
    } else {
        2
    }
}

/// Active overlay modal open over the soundboard UI.
#[derive(Debug, Clone)]
pub enum ActiveModal {
    SoundEditor(SoundModalDraft),
    Settings,
    Alerts,
    CompanionPairing,
}

/// Form draft state for `+` Add Sound and `...` Edit Sound modals.
#[derive(Debug, Clone)]
pub struct SoundModalDraft {
    pub editing_id: Option<String>,
    pub title: String,
    pub audio_path: Option<PathBuf>,
    pub image_path: Option<PathBuf>,
    pub duration_secs: f32,
    pub volume_trim: f32,
    pub trim_start: f32,
    pub trim_end: f32,
    pub waveform_peaks: Option<Arc<Vec<f32>>>,
    pub waveform_loaded_for: Option<PathBuf>,
    pub shortcut: Option<String>,
    pub shortcut_input: String,
    pub recording_shortcut: bool,
    pub shortcut_conflict_notice: Option<String>,
    pub error_message: Option<String>,
    pub youtube_url: String,
    pub youtube_downloading: bool,
    pub youtube_status: Option<String>,
    pub youtube_rx: Option<Arc<Mutex<Receiver<YoutubeDownloadEvent>>>>,
}

impl SoundModalDraft {
    pub fn new_add() -> Self {
        Self {
            editing_id: None,
            title: String::new(),
            audio_path: None,
            image_path: None,
            duration_secs: 0.0,
            volume_trim: 1.0,
            trim_start: 0.0,
            trim_end: 1.0,
            waveform_peaks: None,
            waveform_loaded_for: None,
            shortcut: None,
            shortcut_input: String::new(),
            recording_shortcut: false,
            shortcut_conflict_notice: None,
            error_message: None,
            youtube_url: String::new(),
            youtube_downloading: false,
            youtube_status: None,
            youtube_rx: None,
        }
    }

    pub fn from_existing(item: &SoundItem) -> Self {
        let (trim_start, trim_end) = sanitize_trim_range(item.trim_start, item.trim_end);
        Self {
            editing_id: Some(item.id.clone()),
            title: item.title.clone(),
            audio_path: Some(item.audio_path.clone()),
            image_path: item.image_path.clone(),
            duration_secs: item.duration_secs,
            volume_trim: item.volume_trim,
            trim_start,
            trim_end,
            waveform_peaks: None,
            waveform_loaded_for: None,
            shortcut: item.shortcut.clone(),
            shortcut_input: item.shortcut.clone().unwrap_or_default(),
            recording_shortcut: false,
            shortcut_conflict_notice: None,
            error_message: None,
            youtube_url: String::new(),
            youtube_downloading: false,
            youtube_status: None,
            youtube_rx: None,
        }
    }

    pub fn from_existing_recording_shortcut(item: &SoundItem) -> Self {
        let mut draft = Self::from_existing(item);
        draft.recording_shortcut = true;
        draft
    }
}

/// Actions requested by modal interactions on a single frame.
#[derive(Debug, Default)]
pub struct ModalOutcome {
    pub close_modal: bool,
    pub save_sound: Option<SoundModalDraft>,
    pub apply_audio_config: bool,
    pub refresh_and_apply_devices: bool,
    pub refresh_hotkey_permissions: bool,
    pub open_settings: bool,
    pub config_dirty: bool,
}

/// Renders whichever modal dialog is currently active.
#[allow(clippy::too_many_arguments)]
pub fn render_modals(
    ctx: &egui::Context,
    modal: &mut ActiveModal,
    config: &mut AppConfig,
    audio: &AudioEngine,
    devices: &AudioDeviceList,
    status: &AudioEngineStatus,
    hotkey_status: &HotkeyStatus,
    driver_setup: &DriverSetupState,
    update_state: &UpdateCheckState,
    local_ip: &str,
    connected_clients: usize,
) -> ModalOutcome {
    let mut outcome = ModalOutcome::default();

    match modal {
        ActiveModal::SoundEditor(draft) => {
            render_sound_editor_modal(ctx, draft, config, audio, &mut outcome);
        }
        ActiveModal::Settings => {
            render_settings_modal(
                ctx,
                config,
                devices,
                driver_setup,
                update_state,
                &mut outcome,
            );
        }
        ActiveModal::Alerts => {
            render_alerts_modal(
                ctx,
                &config.theme,
                status,
                hotkey_status,
                devices,
                driver_setup,
                &mut outcome,
            );
        }
        ActiveModal::CompanionPairing => {
            render_companion_modal(
                ctx,
                &config.theme,
                config.audio.sync_port,
                local_ip,
                connected_clients,
                &mut outcome,
            );
        }
    }

    outcome
}

fn keep_modal_on_top(
    ctx: &egui::Context,
    win_resp: Option<egui::InnerResponse<Option<()>>>,
) {
    if let Some(resp) = win_resp {
        ctx.move_to_top(resp.response.layer_id);
    }
}

fn poll_youtube_download(draft: &mut SoundModalDraft, config: &AppConfig) {
    let Some(rx_arc) = draft.youtube_rx.clone() else {
        return;
    };

    let mut finished = false;
    let mut disconnected = false;
    {
        if let Ok(rx) = rx_arc.lock() {
            loop {
                match rx.try_recv() {
                    Ok(YoutubeDownloadEvent::Progress(msg)) => {
                        draft.youtube_status = Some(msg);
                    }
                    Ok(YoutubeDownloadEvent::Completed {
                        audio_path,
                        clean_title,
                        duration_secs,
                        waveform_peaks,
                    }) => {
                        if let Some(prev_audio) = draft.audio_path.take() {
                            config.cleanup_managed_path(&prev_audio);
                        }
                        draft.audio_path = Some(audio_path.clone());
                        draft.duration_secs = duration_secs;
                        draft.trim_start = 0.0;
                        draft.trim_end = 1.0;
                        draft.waveform_peaks = Some(waveform_peaks);
                        draft.waveform_loaded_for = Some(audio_path);
                        if draft.title.trim().is_empty() {
                            draft.title = clean_title;
                        }
                        draft.youtube_downloading = false;
                        draft.youtube_status = None;
                        draft.error_message = None;
                        finished = true;
                        break;
                    }
                    Ok(YoutubeDownloadEvent::Failed(err)) => {
                        draft.youtube_downloading = false;
                        draft.youtube_status = None;
                        draft.error_message = Some(err);
                        finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
    }

    if disconnected && draft.youtube_downloading {
        draft.youtube_downloading = false;
        draft.youtube_status = None;
        if draft.error_message.is_none() {
            draft.error_message =
                Some("YouTube download worker terminated unexpectedly.".to_string());
        }
        finished = true;
    }

    if finished || disconnected {
        draft.youtube_rx = None;
    }
}

fn start_youtube_download(ctx: &egui::Context, draft: &mut SoundModalDraft, audio: &AudioEngine) {
    if draft.youtube_downloading {
        return;
    }
    match yt_downloader::validate_youtube_url(&draft.youtube_url) {
        Ok(normalized) => {
            draft.youtube_url = normalized.clone();
            draft.youtube_downloading = true;
            draft.youtube_status = Some("Downloading & converting audio...".to_string());
            draft.error_message = None;
            let rx = yt_downloader::spawn_youtube_download(
                normalized,
                audio.clone(),
                ctx.clone(),
            );
            draft.youtube_rx = Some(Arc::new(Mutex::new(rx)));
        }
        Err(err) => {
            draft.error_message = Some(err);
        }
    }
}

fn render_sound_editor_modal(
    ctx: &egui::Context,
    draft: &mut SoundModalDraft,
    config: &AppConfig,
    audio: &AudioEngine,
    outcome: &mut ModalOutcome,
) {
    poll_youtube_download(draft, config);

    if draft.audio_path.as_ref() != draft.waveform_loaded_for.as_ref() {
        if let Some(ref path) = draft.audio_path {
            draft.waveform_peaks = audio.waveform_peaks_for_path(path).ok();
            draft.waveform_loaded_for = Some(path.clone());
        } else {
            draft.waveform_peaks = None;
            draft.waveform_loaded_for = None;
        }
    }

    let title = if draft.editing_id.is_some() {
        "Edit Sound Properties"
    } else {
        "Add New Sound"
    };

    let mut open = true;
    let win_resp = egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .frame(themed_modal_frame(ctx, &config.theme))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .default_width(510.0)
        .min_width(500.0)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            egui::Grid::new("sound_editor_grid")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Title:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.title)
                            .hint_text("Sound name...")
                            .desired_width(345.0),
                    );
                    ui.end_row();

                    ui.label("Audio File:");
                    ui.horizontal(|ui| {
                        let audio_label = draft
                            .audio_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "None selected".to_string());
                        ui.add(
                            egui::Label::new(egui::RichText::new(audio_label).monospace())
                                .truncate(),
                        );
                        if ui
                            .add_enabled(!draft.youtube_downloading, egui::Button::new("Browse Audio..."))
                            .clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
                                .pick_file()
                        {
                            if draft.title.trim().is_empty()
                                && let Some(stem) = path.file_stem()
                            {
                                draft.title = stem.to_string_lossy().to_string();
                            }
                            match audio.probe_audio_with_peaks(&path) {
                                Ok((secs, peaks)) => {
                                    if let Some(prev_audio) = draft.audio_path.take()
                                        && prev_audio != path
                                    {
                                        config.cleanup_managed_path(&prev_audio);
                                    }
                                    draft.duration_secs = secs;
                                    draft.trim_start = 0.0;
                                    draft.trim_end = 1.0;
                                    draft.waveform_peaks = Some(peaks);
                                    draft.waveform_loaded_for = Some(path.clone());
                                    draft.audio_path = Some(path);
                                    draft.error_message = None;
                                }
                                Err(err) => {
                                    draft.error_message =
                                        Some(format!("Could not decode audio: {err}"));
                                }
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("YouTube URL:");
                    let mut trigger_yt_download = false;
                    ui.horizontal(|ui| {
                        let url_resp = ui.add_enabled(
                            !draft.youtube_downloading,
                            egui::TextEdit::singleline(&mut draft.youtube_url)
                                .desired_width(258.0)
                                .hint_text("Paste YouTube video or Shorts URL..."),
                        );
                        let can_download =
                            !draft.youtube_url.trim().is_empty() && !draft.youtube_downloading;
                        if ui
                            .add_enabled(can_download, egui::Button::new("Download"))
                            .clicked()
                            || (can_download
                                && url_resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            trigger_yt_download = true;
                        }
                    });
                    if trigger_yt_download {
                        start_youtube_download(ctx, draft, audio);
                    }
                    ui.end_row();

                    if draft.youtube_downloading {
                        ui.label("");
                        ui.horizontal(|ui| {
                            ui.spinner();
                            let status = draft
                                .youtube_status
                                .as_deref()
                                .unwrap_or("Downloading & converting audio...");
                            ui.label(egui::RichText::new(status).weak().size(11.5));
                        });
                        ui.end_row();
                    }

                    ui.label("Cover Image:");
                    ui.horizontal(|ui| {
                        let img_label = draft
                            .image_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| {
                                "Optional (.png / .jpg / .webp / .bmp / .gif)".to_string()
                            });
                        ui.add(
                            egui::Label::new(egui::RichText::new(img_label).weak()).truncate(),
                        );
                        if ui.button("Browse Image...").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter(
                                    "Image Files",
                                    &["png", "jpg", "jpeg", "webp", "bmp", "gif"],
                                )
                                .pick_file()
                        {
                            draft.image_path = Some(path);
                        }
                        if draft.image_path.is_some() && ui.button("Clear").clicked() {
                            draft.image_path = None;
                        }
                    });
                    ui.end_row();

                    ui.label("Sound Volume:");
                    ui.horizontal(|ui| {
                        let mut pct = draft.volume_trim * 100.0;
                        if ui
                            .add(egui::Slider::new(&mut pct, 0.0..=200.0).suffix("%"))
                            .changed()
                        {
                            draft.volume_trim = (pct / 100.0).clamp(0.0, 2.0);
                        }
                    });
                    ui.end_row();

                    ui.label("Sound Trim:");
                    ui.vertical(|ui| {
                        render_waveform_trim_editor(ui, ctx, draft, audio, &config.theme);
                    });
                    ui.end_row();

                    ui.label("Shortcut:");
                    let mut sc_err: Option<String> = None;
                    let shortcut_changed = render_shortcut_picker(
                        ctx,
                        ui,
                        egui::Id::new("sound_editor_shortcut"),
                        &config.theme,
                        &mut draft.shortcut_input,
                        &mut draft.shortcut,
                        &mut draft.recording_shortcut,
                        &mut sc_err,
                    );
                    if let Some(err) = sc_err {
                        draft.error_message = Some(err);
                    } else if shortcut_changed {
                        draft.error_message = None;
                        draft.shortcut_conflict_notice = draft.shortcut.as_deref().and_then(|sc| {
                            config
                                .shortcut_conflict_label(
                                    sc,
                                    draft.editing_id.as_deref().map(ShortcutOwner::Sound),
                                )
                                .map(|owner| {
                                    format!(
                                        "Note: '{sc}' is currently assigned to {owner} and will be reassigned on save."
                                    )
                                })
                        });
                    }
                    ui.end_row();

                    ui.label("Duration:");
                    let (ts, te) = sanitize_trim_range(draft.trim_start, draft.trim_end);
                    let trimmed_total = (draft.duration_secs.max(0.0) * (te - ts)).max(0.0);
                    let t_mins = (trimmed_total.round() as u32) / 60;
                    let t_secs = (trimmed_total.round() as u32) % 60;
                    if ts > 1e-4 || te < 1.0 - 1e-4 {
                        let f_mins = (draft.duration_secs.max(0.0).round() as u32) / 60;
                        let f_secs = (draft.duration_secs.max(0.0).round() as u32) % 60;
                        ui.label(format!("{t_mins:02}:{t_secs:02} (full {f_mins:02}:{f_secs:02})"));
                    } else {
                        ui.label(format!("{t_mins:02}:{t_secs:02}"));
                    }
                    ui.end_row();
                });

            if let Some(notice) = &draft.shortcut_conflict_notice {
                ui.add_space(4.0);
                ui.colored_label(Color32::from_rgb(235, 140, 50), notice);
            }

            if let Some(err) = &draft.error_message {
                ui.add_space(4.0);
                ui.colored_label(Color32::from_rgb(220, 70, 70), err);
            }

            ui.add_space(12.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    outcome.close_modal = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let shortcut_valid = draft.shortcut_input.trim().is_empty()
                        || hotkeys::parse_shortcut(&draft.shortcut_input).is_ok();
                    let can_save =
                        draft.audio_path.is_some() && shortcut_valid && !draft.youtube_downloading;
                    if ui
                        .add_enabled(can_save, primary_accent_button("Save Sound", &config.theme))
                        .clicked()
                    {
                        outcome.save_sound = Some(draft.clone());
                        outcome.close_modal = true;
                    }
                });
            });
        });
    keep_modal_on_top(ctx, win_resp);

    if !open {
        outcome.close_modal = true;
    }
}

fn format_timestamp_tenths(secs: f32) -> String {
    let clamped = secs.max(0.0);
    let tenths_total = (clamped * 10.0).round() as u32;
    let mins = tenths_total / 600;
    let whole_secs = (tenths_total / 10) % 60;
    let tenths = tenths_total % 10;
    format!("{mins:02}:{whole_secs:02}.{tenths}")
}

fn render_waveform_trim_editor(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    draft: &mut SoundModalDraft,
    audio: &AudioEngine,
    theme: &ThemeConfig,
) {
    let (mut trim_start, mut trim_end) = sanitize_trim_range(draft.trim_start, draft.trim_end);
    let accent = rgb_to_color32(theme.accent_color);
    let bg_box = rgb_to_color32(theme.bg_main);
    let border_col = rgb_to_color32(theme.toolbar_divider);
    let muted_bar = rgb_to_color32(theme.icon_secondary).gamma_multiply(0.45);
    let text_col = rgb_to_color32(theme.pad_text);

    let box_size = Vec2::new(350.0, 56.0);
    let Some(ref audio_path) = draft.audio_path.clone() else {
        let (rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(6), bg_box);
        p.rect_stroke(
            rect,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, border_col),
            egui::StrokeKind::Inside,
        );
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Select an audio file to trim waveform",
            egui::FontId::proportional(11.0),
            text_col.gamma_multiply(0.45),
        );
        return;
    };

    #[derive(Clone, Copy, PartialEq, Eq, Default)]
    enum TrimDragHandle {
        #[default]
        None,
        Start,
        End,
    }

    let drag_id = egui::Id::new("sound_editor_trim_drag_handle");
    let mut active_handle: TrimDragHandle =
        ui.data(|d| d.get_temp(drag_id)).unwrap_or_default();

    let (rect, resp) = ui.allocate_exact_size(box_size, egui::Sense::click_and_drag());
    let inner_rect = rect.shrink2(Vec2::new(4.0, 4.0));
    let start_x = inner_rect.min.x + trim_start * inner_rect.width();
    let end_x = inner_rect.min.x + trim_end * inner_rect.width();

    let pointer_x = resp
        .interact_pointer_pos()
        .or(resp.hover_pos())
        .map(|p| p.x);

    if (resp.drag_started() || resp.clicked())
        && let Some(px) = resp.interact_pointer_pos().map(|p| p.x)
    {
        let dist_start = (px - start_x).abs();
        let dist_end = (px - end_x).abs();
        active_handle = if dist_start <= dist_end {
            TrimDragHandle::Start
        } else {
            TrimDragHandle::End
        };
    }

    if active_handle != TrimDragHandle::None && (resp.dragged() || resp.clicked()) {
        if let Some(px) = resp.interact_pointer_pos().map(|p| p.x) {
            let norm = ((px - inner_rect.min.x) / inner_rect.width().max(1.0)).clamp(0.0, 1.0);
            match active_handle {
                TrimDragHandle::Start => {
                    trim_start = norm.clamp(0.0, trim_end - MIN_TRIM_GAP);
                }
                TrimDragHandle::End => {
                    trim_end = norm.clamp(trim_start + MIN_TRIM_GAP, 1.0);
                }
                TrimDragHandle::None => {}
            }
            draft.trim_start = trim_start;
            draft.trim_end = trim_end;
        }
    } else if !resp.dragged() {
        active_handle = TrimDragHandle::None;
    }

    ui.data_mut(|d| d.insert_temp(drag_id, active_handle));

    let near_handle = pointer_x.is_some_and(|px| {
        (px - start_x).abs() <= 9.0 || (px - end_x).abs() <= 9.0
    });
    if active_handle != TrimDragHandle::None || (resp.hovered() && near_handle) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let start_x = inner_rect.min.x + trim_start * inner_rect.width();
    let end_x = inner_rect.min.x + trim_end * inner_rect.width();

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), bg_box);

    // Waveform amplitude bars
    if let Some(ref peaks) = draft.waveform_peaks
        && !peaks.is_empty()
    {
        let bar_count = peaks.len();
        let step = inner_rect.width() / bar_count as f32;
        let bar_w = (step * 0.68).clamp(1.2, 3.0);
        let max_h = inner_rect.height().max(4.0);
        let cy = inner_rect.center().y;

        for (i, &peak) in peaks.iter().enumerate() {
            let cx = inner_rect.min.x + (i as f32 + 0.5) * step;
            let bar_h = (peak.clamp(0.08, 1.0) * max_h).max(3.0);
            let bar_rect = Rect::from_center_size(Pos2::new(cx, cy), Vec2::new(bar_w, bar_h));
            let col = if cx >= start_x && cx <= end_x {
                accent
            } else {
                muted_bar
            };
            painter.rect_filled(bar_rect, CornerRadius::same(1), col);
        }
    } else {
        painter.line_segment(
            [
                Pos2::new(inner_rect.min.x, inner_rect.center().y),
                Pos2::new(inner_rect.max.x, inner_rect.center().y),
            ],
            Stroke::new(1.5_f32, muted_bar),
        );
    }

    // Dimmed dark overlays over trimmed-out outer regions
    let overlay_col = Color32::from_black_alpha(120);
    if start_x > rect.min.x {
        let left_shade = Rect::from_min_max(rect.min, Pos2::new(start_x, rect.max.y));
        painter.rect_filled(left_shade, CornerRadius::ZERO, overlay_col);
    }
    if end_x < rect.max.x {
        let right_shade = Rect::from_min_max(Pos2::new(end_x, rect.min.y), rect.max);
        painter.rect_filled(right_shade, CornerRadius::ZERO, overlay_col);
    }

    // Live preview playhead indicator when previewing inside modal
    let is_previewing =
        audio.is_playing() && audio.active_sound_id().as_deref() == Some(PREVIEW_SOUND_ID);
    if is_previewing {
        let p = audio.playback_progress().clamp(0.0, 1.0);
        let play_norm = trim_start + p * (trim_end - trim_start);
        let play_x = (inner_rect.min.x + play_norm * inner_rect.width())
            .clamp(rect.min.x, rect.max.x);
        painter.line_segment(
            [Pos2::new(play_x, rect.min.y + 2.0), Pos2::new(play_x, rect.max.y - 2.0)],
            Stroke::new(1.8_f32, contrast_on_accent(accent)),
        );
        ctx.request_repaint();
    }

    // Draggable Start & End vertical trim handles
    for (hx, is_active_h) in [
        (start_x, active_handle == TrimDragHandle::Start),
        (end_x, active_handle == TrimDragHandle::End),
    ] {
        let handle_col = if is_active_h {
            contrast_on_accent(accent)
        } else {
            text_col
        };
        painter.line_segment(
            [Pos2::new(hx, rect.min.y), Pos2::new(hx, rect.max.y)],
            Stroke::new(2.2_f32, handle_col),
        );
        let top_tab = Rect::from_center_size(Pos2::new(hx, rect.min.y + 6.0), Vec2::new(6.0, 12.0));
        let bot_tab = Rect::from_center_size(Pos2::new(hx, rect.max.y - 6.0), Vec2::new(6.0, 12.0));
        painter.rect_filled(top_tab, CornerRadius::same(2), accent);
        painter.rect_filled(bot_tab, CornerRadius::same(2), accent);
    }

    // Outer frame stroke cleanly over overlays
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0_f32, border_col),
        egui::StrokeKind::Inside,
    );

    // Timestamps + Preview + Reset Trim controls
    let total_secs = draft.duration_secs.max(0.0);
    let start_secs = trim_start * total_secs;
    let end_secs = trim_end * total_secs;
    let trimmed_secs = (end_secs - start_secs).max(0.0);
    let range_text = format!(
        "{} - {} ({:.1}s)",
        format_timestamp_tenths(start_secs),
        format_timestamp_tenths(end_secs),
        trimmed_secs
    );

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.set_width(box_size.x);
        ui.label(
            egui::RichText::new(range_text)
                .monospace()
                .size(11.0)
                .color(text_col.gamma_multiply(0.85)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let is_trimmed = trim_start > 1e-4 || trim_end < 1.0 - 1e-4;
            if ui
                .add_enabled(is_trimmed, egui::Button::new("Reset Trim"))
                .on_hover_text("Restore full audio range (0% - 100%)")
                .clicked()
            {
                draft.trim_start = 0.0;
                draft.trim_end = 1.0;
            }

            let preview_label = if is_previewing { "Stop" } else { "Preview" };
            if ui
                .button(preview_label)
                .on_hover_text("Audition the trimmed audio slice")
                .clicked()
            {
                if is_previewing {
                    audio.stop();
                } else if let Err(err) = audio.preview_trimmed_file(
                    audio_path,
                    draft.volume_trim,
                    draft.trim_start,
                    draft.trim_end,
                ) {
                    draft.error_message = Some(format!("Preview failed: {err}"));
                }
            }
        });
    });
}

fn blend_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 { ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8 };
    Color32::from_rgb(mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]))
}

#[allow(clippy::too_many_arguments)]
fn render_shortcut_picker(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    id: egui::Id,
    theme: &ThemeConfig,
    shortcut_input: &mut String,
    shortcut_value: &mut Option<String>,
    recording: &mut bool,
    validation_error: &mut Option<String>,
) -> bool {
    let mut changed = false;

    if *recording {
        match hotkeys::capture_egui_shortcut(ctx) {
            ShortcutCapture::Captured(captured) => {
                *recording = false;
                *shortcut_input = captured.clone();
                *shortcut_value = Some(captured);
                *validation_error = None;
                changed = true;
            }
            ShortcutCapture::Cancelled => {
                *recording = false;
            }
            ShortcutCapture::Idle => {}
        }
    }

    let text_color = rgb_to_color32(theme.pad_text);
    let muted_hint = text_color.gamma_multiply(0.45);
    let accent = rgb_to_color32(theme.accent_color);
    let border_muted = rgb_to_color32(theme.toolbar_divider);
    let is_bound = shortcut_value.is_some() && !shortcut_input.trim().is_empty();
    let has_text = !shortcut_input.trim().is_empty() || shortcut_value.is_some();

    ui.horizontal(|ui| {
        if *recording {
            let coral = Color32::from_rgb(235, 110, 60);
            ui.colored_label(coral, egui::RichText::new("Press keys... (Esc cancels)").strong());
            if ui.button("Cancel").clicked() {
                *recording = false;
            }
        } else {
            let field_bg = if is_bound {
                blend_rgb(theme.bg_main, theme.accent_color, 0.14)
            } else {
                rgb_to_color32(theme.bg_main)
            };
            let field_stroke = if is_bound {
                Stroke::new(
                    1.25_f32,
                    Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 195),
                )
            } else {
                Stroke::new(1.0_f32, border_muted)
            };

            let resp = ui
                .scope(|ui| {
                    ui.visuals_mut().widgets.inactive.bg_stroke = field_stroke;
                    ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::new(1.25_f32, accent);
                    ui.add(
                        egui::TextEdit::singleline(shortcut_input)
                            .id(id)
                            .desired_width(146.0)
                            .background_color(field_bg)
                            .text_color(text_color)
                            .hint_text(
                                egui::RichText::new("None (e.g. Ctrl+F1)").color(muted_hint),
                            ),
                    )
                })
                .inner;
            if resp.changed() {
                let trimmed = shortcut_input.trim();
                if trimmed.is_empty() {
                    *shortcut_value = None;
                    *validation_error = None;
                    changed = true;
                } else {
                    match hotkeys::normalize_shortcut(trimmed) {
                        Ok(normalized) => {
                            *shortcut_value = Some(normalized);
                            *validation_error = None;
                            changed = true;
                        }
                        Err(err) => {
                            *validation_error = Some(err);
                        }
                    }
                }
            }
            if ui.button("Record Shortcut").clicked() {
                *recording = true;
                if let Some(focused) = ctx.memory(|m| m.focused()) {
                    ctx.memory_mut(|m| m.surrender_focus(focused));
                }
            }
            if ui
                .add_visible(has_text, egui::Button::new("X"))
                .on_hover_text("Clear shortcut")
                .clicked()
            {
                shortcut_input.clear();
                *shortcut_value = None;
                *validation_error = None;
                changed = true;
            }
        }
    });

    changed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalHotkeySlot {
    StopAll,
    PitchUp,
    PitchDown,
    PitchReset,
}

fn global_shortcut_ref(config: &AppConfig, slot: GlobalHotkeySlot) -> &Option<String> {
    match slot {
        GlobalHotkeySlot::StopAll => &config.stop_shortcut,
        GlobalHotkeySlot::PitchUp => &config.pitch_up_shortcut,
        GlobalHotkeySlot::PitchDown => &config.pitch_down_shortcut,
        GlobalHotkeySlot::PitchReset => &config.pitch_reset_shortcut,
    }
}

fn global_shortcut_mut(config: &mut AppConfig, slot: GlobalHotkeySlot) -> &mut Option<String> {
    match slot {
        GlobalHotkeySlot::StopAll => &mut config.stop_shortcut,
        GlobalHotkeySlot::PitchUp => &mut config.pitch_up_shortcut,
        GlobalHotkeySlot::PitchDown => &mut config.pitch_down_shortcut,
        GlobalHotkeySlot::PitchReset => &mut config.pitch_reset_shortcut,
    }
}

fn render_settings_modal(
    ctx: &egui::Context,
    config: &mut AppConfig,
    devices: &AudioDeviceList,
    driver_setup: &DriverSetupState,
    update_state: &UpdateCheckState,
    outcome: &mut ModalOutcome,
) {
    let mut open = true;
    let win_resp = egui::Window::new("WaweFX Settings")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .frame(themed_modal_frame(ctx, &config.theme))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .default_size(Vec2::new(540.0, 420.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // --- Section 1: Audio Routing Devices ---
                ui.heading("Audio Devices & Routing");
                ui.add_space(4.0);

                if !has_virtual_audio_device(devices) {
                    render_virtual_driver_setup_banner(ui, driver_setup, outcome);
                    ui.add_space(8.0);
                }

                egui::Grid::new("settings_audio_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Physical Mic Input:");
                        let mic_label = config
                            .audio
                            .physical_mic_device
                            .as_deref()
                            .unwrap_or("System Default Input");
                        egui::ComboBox::from_id_salt("cfg_phys_mic")
                            .width(280.0)
                            .selected_text(mic_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        config.audio.physical_mic_device.is_none(),
                                        "System Default Input",
                                    )
                                    .clicked()
                                {
                                    config.audio.physical_mic_device = None;
                                    outcome.config_dirty = true;
                                }
                                for dev in &devices.input_devices {
                                    let selected =
                                        config.audio.physical_mic_device.as_deref() == Some(dev);
                                    if ui.selectable_label(selected, dev).clicked() {
                                        config.audio.physical_mic_device = Some(dev.clone());
                                        outcome.config_dirty = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Virtual Mic Output:");
                        let vmic_label = config
                            .audio
                            .virtual_mic_output_device
                            .as_deref()
                            .unwrap_or("None (Not Configured)");
                        let mut sorted_vmic_outputs = devices.output_devices.clone();
                        sorted_vmic_outputs.sort_by(|a, b| {
                            virtual_mic_priority(a)
                                .cmp(&virtual_mic_priority(b))
                                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
                        });
                        egui::ComboBox::from_id_salt("cfg_virtual_mic")
                            .width(280.0)
                            .selected_text(vmic_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        config.audio.virtual_mic_output_device.is_none(),
                                        "None (Not Configured)",
                                    )
                                    .clicked()
                                {
                                    config.audio.virtual_mic_output_device = None;
                                    outcome.config_dirty = true;
                                }
                                for dev in &sorted_vmic_outputs {
                                    let selected = config.audio.virtual_mic_output_device.as_deref()
                                        == Some(dev);
                                    let display_name = if virtual_mic_priority(dev) < 2 {
                                        format!("★ {dev}")
                                    } else {
                                        dev.clone()
                                    };
                                    if ui.selectable_label(selected, display_name).clicked() {
                                        config.audio.virtual_mic_output_device = Some(dev.clone());
                                        outcome.config_dirty = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("");
                        ui.small(virtual_mic_output_tip());
                        ui.end_row();

                        ui.label("Headphones Output:");
                        let hp_label = config
                            .audio
                            .headphones_output_device
                            .as_deref()
                            .unwrap_or("System Default Output");
                        egui::ComboBox::from_id_salt("cfg_headphones")
                            .width(280.0)
                            .selected_text(hp_label)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        config.audio.headphones_output_device.is_none(),
                                        "System Default Output",
                                    )
                                    .clicked()
                                {
                                    config.audio.headphones_output_device = None;
                                    outcome.config_dirty = true;
                                }
                                for dev in &devices.output_devices {
                                    let selected = config.audio.headphones_output_device.as_deref()
                                        == Some(dev);
                                    if ui.selectable_label(selected, dev).clicked() {
                                        config.audio.headphones_output_device = Some(dev.clone());
                                        outcome.config_dirty = true;
                                    }
                                }
                            });
                        ui.end_row();
                    });

                ui.add_space(4.0);
                if ui
                    .checkbox(
                        &mut config.audio.mic_passthrough_enabled,
                        "Pass physical microphone voice into Virtual Mic output",
                    )
                    .changed()
                {
                    outcome.config_dirty = true;
                }

                ui.add_space(6.0);
                let data_err_id = egui::Id::new("wawefx_open_data_dir_err");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(primary_accent_button("Apply & Restart Streams", &config.theme))
                        .clicked()
                    {
                        outcome.apply_audio_config = true;
                        outcome.config_dirty = true;
                    }
                    if ui.button("Refresh & Apply Devices").clicked() {
                        outcome.refresh_and_apply_devices = true;
                    }
                    if ui
                        .button("Open Data Folder")
                        .on_hover_text(format!("Open {}", AppConfig::data_dir().display()))
                        .clicked()
                    {
                        let res = AppConfig::open_data_folder().err().map(|e| format!("{e:#}"));
                        ctx.data_mut(|d| d.insert_temp(data_err_id, res));
                    }
                });
                if let Some(err) = ctx.data_mut(|d| d.get_temp::<Option<String>>(data_err_id).flatten()) {
                    ui.colored_label(Color32::from_rgb(220, 70, 70), err);
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);

                // --- Section 2: Global Hotkeys (Stop All & Pitch Control) ---
                ui.heading("Global Hotkeys");
                ui.add_space(4.0);

                const GLOBAL_HOTKEY_ROWS: [(&str, GlobalHotkeySlot, ShortcutOwner<'static>, &str); 4] = [
                    ("Stop All Sounds:", GlobalHotkeySlot::StopAll, ShortcutOwner::StopAll, "stop"),
                    ("Pitch Up (+):", GlobalHotkeySlot::PitchUp, ShortcutOwner::PitchUp, "pitch_up"),
                    ("Pitch Down (-):", GlobalHotkeySlot::PitchDown, ShortcutOwner::PitchDown, "pitch_down"),
                    ("Pitch Reset (1.00x):", GlobalHotkeySlot::PitchReset, ShortcutOwner::PitchReset, "pitch_reset"),
                ];

                let rec_slot_key = egui::Id::new("wawefx_settings_hotkey_rec_slot");
                let note_key = egui::Id::new("wawefx_settings_hotkey_note");
                let mut rec_slot: Option<GlobalHotkeySlot> = ctx
                    .data_mut(|d| d.get_temp::<Option<GlobalHotkeySlot>>(rec_slot_key))
                    .flatten();
                let mut global_note: Option<String> = ctx
                    .data_mut(|d| d.get_temp::<Option<String>>(note_key))
                    .flatten();
                let mut pending_assignment: Option<(GlobalHotkeySlot, ShortcutOwner<'static>, Option<String>)> = None;
                let mut validation_errors: Vec<String> = Vec::new();

                egui::Grid::new("settings_global_hotkeys_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        for &(label, slot, owner, tag) in &GLOBAL_HOTKEY_ROWS {
                            ui.label(label);
                            let input_key = egui::Id::new(("wawefx_ghk_input", tag));
                            let err_key = egui::Id::new(("wawefx_ghk_err", tag));

                            let stored_val = global_shortcut_ref(config, slot).clone();
                            let was_recording = rec_slot == Some(slot);
                            let mut recording = was_recording;
                            let mut input_buf: String = ctx.data_mut(|d| {
                                d.get_temp(input_key)
                                    .unwrap_or_else(|| stored_val.clone().unwrap_or_default())
                            });
                            let mut slot_err: Option<String> = ctx
                                .data_mut(|d| d.get_temp::<Option<String>>(err_key))
                                .flatten();

                            if !recording
                                && slot_err.is_none()
                                && stored_val.as_deref() != Some(input_buf.trim())
                                && (stored_val.is_none()
                                    && !input_buf.is_empty()
                                    && hotkeys::parse_shortcut(&input_buf).is_ok()
                                    || stored_val.is_some() && input_buf.is_empty())
                            {
                                input_buf = stored_val.clone().unwrap_or_default();
                            }

                            let mut next_val = stored_val;
                            if render_shortcut_picker(
                                ctx,
                                ui,
                                egui::Id::new(("settings_shortcut_picker", tag)),
                                &config.theme,
                                &mut input_buf,
                                &mut next_val,
                                &mut recording,
                                &mut slot_err,
                            ) {
                                pending_assignment = Some((slot, owner, next_val));
                            }

                            if recording && !was_recording {
                                rec_slot = Some(slot);
                            } else if !recording && was_recording {
                                rec_slot = None;
                            }

                            if let Some(ref err) = slot_err {
                                validation_errors.push(format!("{label} {err}"));
                            }

                            ctx.data_mut(|d| {
                                d.insert_temp(input_key, input_buf);
                                d.insert_temp(err_key, slot_err);
                            });
                            ui.end_row();
                        }

                        ui.label("Pitch Hotkey Step:");
                        ui.horizontal(|ui| {
                            let slider = egui::Slider::new(
                                &mut config.audio.pitch_step,
                                MIN_PITCH_STEP..=MAX_PITCH_STEP,
                            )
                            .step_by(0.01)
                            .fixed_decimals(2)
                            .suffix("x");
                            if ui.add(slider).changed() {
                                outcome.config_dirty = true;
                            }
                            for preset_step in [0.05f32, 0.10, 0.25] {
                                let selected = (config.audio.pitch_step - preset_step).abs() < 0.005;
                                if ui
                                    .selectable_label(selected, format!("{preset_step:.2}x"))
                                    .clicked()
                                {
                                    config.audio.pitch_step = preset_step;
                                    outcome.config_dirty = true;
                                }
                            }
                        });
                        ui.end_row();
                    });

                if let Some((slot, owner, next_val)) = pending_assignment {
                    *global_shortcut_mut(config, slot) = next_val.clone();
                    global_note = if let Some(ref sc) = next_val {
                        let unbound = config.unbind_shortcut_conflicts(sc, Some(owner));
                        if unbound.is_empty() {
                            None
                        } else {
                            Some(format!("Unbound '{sc}' from: {}.", unbound.join(", ")))
                        }
                    } else {
                        None
                    };
                    outcome.config_dirty = true;
                }

                if let Some(ref note) = global_note {
                    ui.colored_label(Color32::from_rgb(235, 140, 50), note);
                }
                for err in &validation_errors {
                    ui.colored_label(Color32::from_rgb(220, 70, 70), err);
                }
                ui.small("Trigger Stop All, Pitch Up/Down/Reset, or any sound pad from anywhere on your system (e.g. Ctrl+Shift+1, F9–F24, Numpad0).");

                ctx.data_mut(|d| {
                    d.insert_temp(rec_slot_key, rec_slot);
                    d.insert_temp(note_key, global_note);
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);

                // --- Section 3: Color Themes (Built-in + User Presets + Custom RGB Pickers) ---
                ui.heading("Theme & Colors");
                ui.add_space(4.0);

                let mut delete_preset_idx: Option<usize> = None;
                let user_presets_snapshot = config.user_presets.clone();
                ui.horizontal_wrapped(|ui| {
                    for preset in ThemePreset::BUILTIN_PRESETS {
                        let active = config.theme.preset == preset
                            && config.theme.active_user_preset.is_none();
                        if ui.selectable_label(active, preset.label()).clicked() {
                            config.theme.apply_preset(preset);
                            outcome.config_dirty = true;
                        }
                    }
                    for (idx, named) in user_presets_snapshot.iter().enumerate() {
                        let active = config
                            .theme
                            .active_user_preset
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&named.name));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if ui.selectable_label(active, &named.name).clicked() {
                                config.theme.apply_named_preset(named);
                                outcome.config_dirty = true;
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text(format!("Delete preset '{}'", named.name))
                                .clicked()
                            {
                                delete_preset_idx = Some(idx);
                            }
                        });
                    }
                    if config.theme.preset == ThemePreset::Custom
                        && config.theme.active_user_preset.is_none()
                    {
                        ui.label(egui::RichText::new("• Custom").strong());
                    }
                });
                if let Some(idx) = delete_preset_idx
                    && config.delete_user_preset(idx)
                {
                    outcome.config_dirty = true;
                }

                ui.add_space(8.0);
                let mut color_changed = false;
                egui::Grid::new("settings_color_grid")
                    .num_columns(4)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        color_changed |= color_row(ui, "Main Background", &mut config.theme.bg_main);
                        color_changed |= color_row(ui, "Toolbar Background", &mut config.theme.bg_toolbar);
                        ui.end_row();

                        color_changed |= color_row(ui, "Toolbar Divider", &mut config.theme.toolbar_divider);
                        color_changed |= color_row(ui, "Divider Shadow", &mut config.theme.toolbar_shadow);
                        ui.end_row();

                        color_changed |= color_row(ui, "Primary Icons", &mut config.theme.icon_primary);
                        color_changed |= color_row(ui, "Rewind / FF Icons", &mut config.theme.icon_secondary);
                        ui.end_row();

                        color_changed |= color_row(ui, "Phone Icon", &mut config.theme.icon_phone);
                        color_changed |= color_row(ui, "Knobs & Track", &mut config.theme.gauge_and_track);
                        ui.end_row();

                        color_changed |= color_row(ui, "Primary Accent", &mut config.theme.accent_color);
                        color_changed |= color_row(ui, "Pad Surface", &mut config.theme.pad_card_bg);
                        ui.end_row();

                        color_changed |= color_row(ui, "Pad Hover", &mut config.theme.pad_card_hover);
                        color_changed |= color_row(ui, "Pad Text", &mut config.theme.pad_text);
                        ui.end_row();
                    });

                if color_changed {
                    config
                        .theme
                        .mark_custom_if_changed_with_presets(&config.user_presets);
                    outcome.config_dirty = true;
                }

                ui.add_space(8.0);
                let new_preset_id = egui::Id::new("wawefx_new_theme_preset_name");
                let mut preset_name: String = ctx
                    .data_mut(|d| d.get_temp::<String>(new_preset_id))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label("Save Current Colors:");
                    let edit_resp = ui.add(
                        egui::TextEdit::singleline(&mut preset_name)
                            .id(new_preset_id.with("input"))
                            .hint_text("Preset name...")
                            .desired_width(180.0),
                    );
                    let can_save = !preset_name.trim().is_empty();
                    let enter_submitted = can_save
                        && edit_resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let btn_clicked = ui
                        .add_enabled(can_save, primary_accent_button("Save Preset", &config.theme))
                        .clicked();
                    if (btn_clicked || enter_submitted) && config.save_user_preset(&preset_name) {
                        preset_name.clear();
                        outcome.config_dirty = true;
                    }
                });
                ctx.data_mut(|d| d.insert_temp(new_preset_id, preset_name));

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);

                // --- Section 4: Version & Updates ---
                ui.heading("About & Updates");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(format!("WaweFX v{CURRENT_VERSION}"))
                            .strong(),
                    );
                    ui.small(format!("({GITHUB_REPO_SLUG})"));
                    ui.label("•");
                    match update_state {
                        UpdateCheckState::Checking => {
                            ui.small("Checking GitHub Releases...");
                        }
                        UpdateCheckState::UpToDate => {
                            ui.colored_label(Color32::from_rgb(46, 160, 67), "Up to date");
                        }
                        UpdateCheckState::UpdateAvailable(info) => {
                            let accent = rgb_to_color32(config.theme.accent_color);
                            ui.colored_label(
                                accent,
                                egui::RichText::new(format!("New version available: v{}", info.version))
                                    .strong(),
                            );
                            if ui
                                .add(primary_accent_button("Open Release Page", &config.theme))
                                .clicked()
                            {
                                updater::open_release_url(&info.url);
                            }
                        }
                        UpdateCheckState::OfflineOrUnavailable => {
                            ui.small("Offline / GitHub check skipped");
                        }
                    }
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Reset to Midnight Dark").clicked() {
                        config.theme.apply_preset(ThemePreset::MidnightDark);
                        outcome.config_dirty = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(primary_accent_button("Done", &config.theme)).clicked() {
                            outcome.close_modal = true;
                        }
                    });
                });
            });
        });
    keep_modal_on_top(ctx, win_resp);

    if !open {
        outcome.close_modal = true;
    }
}

fn virtual_mic_output_tip() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Tip: Select a macOS virtual audio device (e.g. BlackHole 2ch / Loopback / Soundflower) so sounds inject into its virtual mic."
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Tip: Select a virtual audio cable playback endpoint (e.g. CABLE Input / VoiceMeeter / BlackHole) so sounds inject into its virtual mic."
    }
}

fn virtual_mic_priority(name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("vb-audio")
        || lower.contains("cable input")
        || lower.contains("voicemeeter")
        || lower.contains("blackhole")
        || lower.contains("loopback")
        || lower.contains("soundflower")
    {
        0
    } else if !lower.contains("steam") && (lower.contains("cable") || lower.contains("virtual")) {
        1
    } else {
        2
    }
}

fn color_row(ui: &mut egui::Ui, label: &str, rgb: &mut [u8; 3]) -> bool {
    ui.label(label);
    ui.color_edit_button_srgb(rgb).changed()
}

fn render_alerts_modal(
    ctx: &egui::Context,
    theme: &ThemeConfig,
    status: &AudioEngineStatus,
    hotkey_status: &HotkeyStatus,
    devices: &AudioDeviceList,
    driver_setup: &DriverSetupState,
    outcome: &mut ModalOutcome,
) {
    let mut open = true;
    let missing_virtual_driver = !has_virtual_audio_device(devices);
    let has_any_warning = !status.warnings.is_empty()
        || !hotkey_status.warnings.is_empty()
        || missing_virtual_driver;
    let win_resp = egui::Window::new("Audio Engine Status & Alerts")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .frame(themed_modal_frame(ctx, theme))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .default_size(Vec2::new(510.0, if missing_virtual_driver { 440.0 } else { 350.0 }))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if missing_virtual_driver {
                    render_virtual_driver_setup_banner(ui, driver_setup, outcome);
                    ui.add_space(8.0);
                }

                ui.label(egui::RichText::new(format!("Backend: {}", status.backend_name)).strong());
                ui.add_space(4.0);
                ui.label(format!(
                    "Physical Mic: {}",
                    status.active_mic_input.as_deref().unwrap_or("Inactive")
                ));
                ui.label(format!(
                    "Virtual Mic Output: {}",
                    status
                        .active_virtual_mic_output
                        .as_deref()
                        .unwrap_or("Not selected")
                ));
                ui.label(format!(
                    "Headphones Output: {}",
                    status
                        .active_headphones_output
                        .as_deref()
                        .unwrap_or("Unavailable")
                ));
                ui.label(format!(
                    "Mic Ring Buffer: {:.1} ms  |  Measured Rate: In {} fps / V-Mic Out {} fps",
                    status.mic_ring_fill_ms,
                    status.measured_mic_in_fps,
                    status.measured_vmic_out_fps
                ));
                ui.label(format!(
                    "Global Hotkeys: {} ({} active)",
                    if hotkey_status.manager_ready { "Active" } else { "Unavailable" },
                    hotkey_status.registered_count
                ));

                if let Some(trusted) = hotkey_status.macos_accessibility_trusted {
                    ui.horizontal(|ui| {
                        if trusted {
                            ui.colored_label(
                                Color32::from_rgb(46, 160, 67),
                                "macOS Accessibility Permission: Granted",
                            );
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(235, 110, 60),
                                "macOS Accessibility Permission: Required for global hotkeys",
                            );
                            if ui.button("Open Accessibility Settings").clicked() {
                                hotkeys::open_macos_accessibility_settings();
                            }
                            if ui.button("Recheck").clicked() {
                                outcome.refresh_hotkey_permissions = true;
                            }
                        }
                    });
                }

                ui.add_space(8.0);
                ui.separator();

                if !has_any_warning {
                    ui.colored_label(
                        Color32::from_rgb(46, 160, 67),
                        "All active audio streams and global hotkeys are running normally.",
                    );
                } else {
                    ui.label(egui::RichText::new("Warnings:").strong());
                    if missing_virtual_driver {
                        ui.colored_label(
                            Color32::from_rgb(235, 110, 60),
                            "• No virtual audio driver (VB-Cable / VoiceMeeter / BlackHole / Loopback) detected.",
                        );
                    }
                    for warning in &status.warnings {
                        ui.colored_label(Color32::from_rgb(235, 110, 60), format!("• {warning}"));
                    }
                    for warning in &hotkey_status.warnings {
                        ui.colored_label(Color32::from_rgb(235, 110, 60), format!("• {warning}"));
                    }
                }

                ui.add_space(8.0);
                ui.small(apo_installer::query_apo_status().notes);

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(primary_accent_button("Open Audio Settings", theme))
                        .clicked()
                    {
                        outcome.open_settings = true;
                    }
                    if ui.button("Refresh & Apply Devices").clicked() {
                        outcome.refresh_and_apply_devices = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            outcome.close_modal = true;
                        }
                    });
                });
            });
        });
    keep_modal_on_top(ctx, win_resp);

    if !open {
        outcome.close_modal = true;
    }
}

fn render_virtual_driver_setup_banner(
    ui: &mut egui::Ui,
    driver_setup: &DriverSetupState,
    outcome: &mut ModalOutcome,
) {
    let coral = Color32::from_rgb(235, 110, 60);
    egui::Frame::NONE
        .fill(Color32::from_rgba_unmultiplied(255, 127, 80, 28))
        .stroke(Stroke::new(1.5_f32, coral))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Virtual Audio Driver Required")
                    .strong()
                    .color(coral),
            );
            ui.add_space(3.0);

            #[cfg(target_os = "windows")]
            {
                ui.small(
                    "No VB-Cable or VoiceMeeter output device was detected. Install VB-Cable so Discord/apps can hear your soundboard & mic mix.",
                );
                ui.add_space(6.0);
                let running = driver_setup.is_running();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!running, egui::Button::new("Install VB-Cable Driver"))
                        .clicked()
                    {
                        driver_setup.start_driver_install();
                    }
                    if ui.button("Open VB-Cable Website").clicked() {
                        driver_setup.open_driver_website();
                    }
                    if ui.button("Refresh & Apply Devices").clicked() {
                        outcome.refresh_and_apply_devices = true;
                    }
                });
            }

            #[cfg(target_os = "macos")]
            {
                ui.small(
                    "No BlackHole or Loopback virtual audio device was detected. Follow these 2 steps:",
                );
                ui.small("1. Click \"Install BlackHole 2ch\" (uses Homebrew if installed, or opens existential.audio).");
                ui.small("2. Click \"Refresh & Apply Devices\" below to auto-select BlackHole 2ch.");
                ui.add_space(6.0);
                let running = driver_setup.is_running();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!running, egui::Button::new("Install BlackHole 2ch"))
                        .clicked()
                    {
                        driver_setup.start_driver_install();
                    }
                    if ui.button("Open Download Page").clicked() {
                        driver_setup.open_driver_website();
                    }
                    if ui.button("Refresh & Apply Devices").clicked() {
                        outcome.refresh_and_apply_devices = true;
                    }
                });
            }

            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                ui.small(
                    "No virtual audio sink detected. Create a PipeWire/PulseAudio virtual sink, then click Refresh & Apply Devices.",
                );
                ui.add_space(6.0);
                if ui.button("Refresh & Apply Devices").clicked() {
                    outcome.refresh_and_apply_devices = true;
                }
            }

            match driver_setup.phase() {
                DriverInstallPhase::Idle => {}
                DriverInstallPhase::Running(msg) => {
                    ui.add_space(5.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(msg);
                    });
                }
                DriverInstallPhase::Succeeded(msg) => {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::from_rgb(46, 160, 67), msg);
                }
                DriverInstallPhase::Failed(msg) => {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::from_rgb(220, 60, 60), msg);
                }
            }
        });
}

fn render_companion_modal(
    ctx: &egui::Context,
    theme: &ThemeConfig,
    sync_port: u16,
    local_ip: &str,
    connected_clients: usize,
    outcome: &mut ModalOutcome,
) {
    let mut open = true;
    let url = if sync_port == 0 {
        format!("http://{local_ip}/")
    } else {
        format!("http://{local_ip}:{sync_port}/")
    };

    let win_resp = egui::Window::new("Android / Mobile Companion Sync")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .frame(themed_modal_frame(ctx, theme))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(340.0, 365.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.label("Scan on your phone to open the live remote:");
                ui.add_space(6.0);

                match QrCode::new(url.as_bytes()) {
                    Ok(code) => paint_qr_code(ui, &code, 184.0),
                    Err(err) => {
                        ui.colored_label(
                            Color32::from_rgb(220, 70, 70),
                            format!("QR error: {err}"),
                        );
                    }
                }

                ui.add_space(8.0);
                ui.label(egui::RichText::new(&url).monospace().strong());
                ui.add_space(4.0);
                let client_color = if connected_clients > 0 {
                    Color32::from_rgb(46, 160, 67)
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(
                    client_color,
                    format!("Connected Clients: {connected_clients}"),
                );
                if sync_port == 0 {
                    ui.colored_label(
                        Color32::from_rgb(220, 70, 70),
                        "Companion server port could not be bound.",
                    );
                } else {
                    ui.small(format!(
                        "Listening on 0.0.0.0:{sync_port} — real-time WebSocket sync active."
                    ));
                }
                ui.add_space(10.0);
                if ui.button("Close").clicked() {
                    outcome.close_modal = true;
                }
            });
        });
    keep_modal_on_top(ctx, win_resp);

    if !open {
        outcome.close_modal = true;
    }
}

fn paint_qr_code(ui: &mut egui::Ui, code: &QrCode, size_px: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size_px), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), Color32::WHITE);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, Color32::from_gray(200)),
        egui::StrokeKind::Inside,
    );

    let width = code.width();
    if width == 0 {
        return;
    }

    let quiet = 2usize;
    let total_modules = width + quiet * 2;
    let cell = size_px / total_modules as f32;
    let colors = code.to_colors();

    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == QrColor::Dark {
                let min = Pos2::new(
                    rect.min.x + (x + quiet) as f32 * cell,
                    rect.min.y + (y + quiet) as f32 * cell,
                );
                let mod_rect = Rect::from_min_size(min, Vec2::splat(cell.ceil()));
                painter.rect_filled(mod_rect, 0.0, Color32::BLACK);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn run_platform_driver_install(phase: &Arc<Mutex<DriverInstallPhase>>) {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let temp = std::env::temp_dir();
    let zip_path = temp.join("VBCABLE_Driver_Pack43.zip");
    let extract_dir = temp.join("WaweFX_VBCable");

    DriverSetupState::set_phase(
        phase,
        DriverInstallPhase::Running(
            "Downloading and extracting VBCABLE_Driver_Pack43.zip...".to_string(),
        ),
    );

    let dl_script = "$ErrorActionPreference = 'Stop'; \
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
        Invoke-WebRequest -Uri $env:WAWEFX_ZIP_URL -OutFile $env:WAWEFX_ZIP_PATH -UseBasicParsing; \
        if (Test-Path -LiteralPath $env:WAWEFX_EXTRACT_DIR) { Remove-Item -LiteralPath $env:WAWEFX_EXTRACT_DIR -Recurse -Force }; \
        Expand-Archive -LiteralPath $env:WAWEFX_ZIP_PATH -DestinationPath $env:WAWEFX_EXTRACT_DIR -Force";

    let dl_out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", dl_script])
        .env("WAWEFX_ZIP_URL", VBCABLE_ZIP_URL)
        .env("WAWEFX_ZIP_PATH", &zip_path)
        .env("WAWEFX_EXTRACT_DIR", &extract_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match dl_out {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("exit code {:?}", out.status.code())
            } else {
                stderr
            };
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Failed(format!(
                    "Download/extract failed ({detail}). Try 'Open VB-Cable Website' instead."
                )),
            );
            return;
        }
        Err(err) => {
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Failed(format!(
                    "Failed to launch PowerShell: {err}. Try 'Open VB-Cable Website' instead."
                )),
            );
            return;
        }
    }

    let Some(setup_exe) = find_file_recursive(&extract_dir, "VBCABLE_Setup_x64.exe") else {
        DriverSetupState::set_phase(
            phase,
            DriverInstallPhase::Failed(
                "VBCABLE_Setup_x64.exe was not found in extracted zip. Try 'Open VB-Cable Website'."
                    .to_string(),
            ),
        );
        return;
    };

    DriverSetupState::set_phase(
        phase,
        DriverInstallPhase::Running(
            "Waiting for UAC elevation & VB-Cable installer to finish...".to_string(),
        ),
    );

    let run_script =
        "$ErrorActionPreference = 'Stop'; Start-Process -FilePath $env:WAWEFX_SETUP_EXE -Verb RunAs -Wait";
    let setup_out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", run_script])
        .env("WAWEFX_SETUP_EXE", &setup_exe)
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match setup_out {
        Ok(out) if out.status.success() => {
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Succeeded(
                    "VB-Cable installer finished. Click 'Refresh & Apply Devices' (reboot Windows if 'CABLE Input' does not appear yet).".to_string(),
                ),
            );
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Failed(format!(
                    "Installer did not complete ({}).",
                    if stderr.is_empty() {
                        "UAC declined or cancelled".to_string()
                    } else {
                        stderr
                    }
                )),
            );
        }
        Err(err) => {
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Failed(format!("Could not launch elevated installer: {err}")),
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn find_file_recursive(dir: &std::path::Path, target_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(target_name))
        {
            return Some(path);
        } else if path.is_dir()
            && let Some(found) = find_file_recursive(&path, target_name)
        {
            return Some(found);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn open_platform_driver_website() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("cmd")
        .args(["/C", "start", "", VBCABLE_WEB_URL])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn detect_homebrew_binary() -> Option<&'static str> {
    use std::process::Command;

    for candidate in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew", "brew"] {
        if let Ok(out) = Command::new(candidate).arg("--version").output()
            && out.status.success()
        {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn run_platform_driver_install(phase: &Arc<Mutex<DriverInstallPhase>>) {
    use std::process::Command;

    if let Some(brew) = detect_homebrew_binary() {
        DriverSetupState::set_phase(
            phase,
            DriverInstallPhase::Running(
                "Installing blackhole-2ch via Homebrew (brew install blackhole-2ch)...".to_string(),
            ),
        );
        match Command::new(brew).args(["install", "blackhole-2ch"]).output() {
            Ok(out) if out.status.success() => {
                DriverSetupState::set_phase(
                    phase,
                    DriverInstallPhase::Succeeded(
                        "BlackHole 2ch installed! Click 'Refresh & Apply Devices' above."
                            .to_string(),
                    ),
                );
                return;
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let _ = open_platform_driver_website();
                DriverSetupState::set_phase(
                    phase,
                    DriverInstallPhase::Failed(format!(
                        "Homebrew install failed ({stderr}). Opened existential.audio download page instead."
                    )),
                );
                return;
            }
            Err(err) => {
                DriverSetupState::set_phase(
                    phase,
                    DriverInstallPhase::Failed(format!("Failed to invoke Homebrew: {err}")),
                );
                return;
            }
        }
    }

    match open_platform_driver_website() {
        Ok(()) => {
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Succeeded(
                    "Homebrew not detected; opened https://existential.audio/blackhole/. Run the installer, then click 'Refresh & Apply Devices'."
                        .to_string(),
                ),
            );
        }
        Err(err) => {
            DriverSetupState::set_phase(
                phase,
                DriverInstallPhase::Failed(format!("Could not open browser: {err}")),
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn open_platform_driver_website() -> std::io::Result<()> {
    std::process::Command::new("open")
        .arg(BLACKHOLE_WEB_URL)
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn run_platform_driver_install(phase: &Arc<Mutex<DriverInstallPhase>>) {
    DriverSetupState::set_phase(
        phase,
        DriverInstallPhase::Failed(
            "Automated driver installation is only supported on Windows and macOS.".to_string(),
        ),
    );
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_platform_driver_website() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::audio::AudioDeviceList;

    use super::{
        find_preferred_virtual_mic_output, has_virtual_audio_device, virtual_mic_priority,
    };

    #[test]
    fn prioritizes_windows_and_macos_virtual_audio_drivers() {
        assert_eq!(virtual_mic_priority("CABLE Input (VB-Audio Virtual Cable)"), 0);
        assert_eq!(virtual_mic_priority("VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"), 0);
        assert_eq!(virtual_mic_priority("BlackHole 2ch"), 0);
        assert_eq!(virtual_mic_priority("Rogue Amoeba Loopback Audio"), 0);
        assert_eq!(virtual_mic_priority("Soundflower (2ch)"), 0);

        assert_eq!(virtual_mic_priority("Generic Virtual Audio Device"), 1);
        assert_eq!(virtual_mic_priority("Steam Streaming Speakers"), 2);
        assert_eq!(virtual_mic_priority("MacBook Pro Speakers"), 2);
    }

    #[test]
    fn detects_and_selects_virtual_audio_drivers_across_windows_and_macos() {
        let none_list = AudioDeviceList {
            input_devices: vec!["Built-in Microphone".to_string()],
            output_devices: vec![
                "Speakers (Realtek High Definition Audio)".to_string(),
                "Steam Streaming Speakers".to_string(),
            ],
        };
        assert!(!has_virtual_audio_device(&none_list));
        assert_eq!(find_preferred_virtual_mic_output(&none_list), None);

        let win_list = AudioDeviceList {
            input_devices: vec![],
            output_devices: vec![
                "Speakers (Realtek)".to_string(),
                "VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)".to_string(),
                "CABLE Input (VB-Audio Virtual Cable)".to_string(),
            ],
        };
        assert!(has_virtual_audio_device(&win_list));
        assert_eq!(
            find_preferred_virtual_mic_output(&win_list).as_deref(),
            Some("CABLE Input (VB-Audio Virtual Cable)")
        );

        let mac_list = AudioDeviceList {
            input_devices: vec![],
            output_devices: vec![
                "MacBook Pro Speakers".to_string(),
                "Loopback Audio".to_string(),
                "BlackHole 2ch".to_string(),
            ],
        };
        assert!(has_virtual_audio_device(&mac_list));
        assert_eq!(
            find_preferred_virtual_mic_output(&mac_list).as_deref(),
            Some("BlackHole 2ch")
        );
    }
}
