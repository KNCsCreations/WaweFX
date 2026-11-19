use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use qrcode::{Color as QrColor, QrCode};

use crate::audio::{AudioDeviceList, AudioEngine, AudioEngineStatus, apo_installer};
use crate::config::{
    AppConfig, MAX_PITCH_STEP, MIN_PITCH_STEP, ShortcutOwner, SoundItem, ThemeConfig, ThemePreset,
    rgb_to_color32,
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
    pub shortcut: Option<String>,
    pub shortcut_input: String,
    pub recording_shortcut: bool,
    pub shortcut_conflict_notice: Option<String>,
    pub error_message: Option<String>,
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
            shortcut: None,
            shortcut_input: String::new(),
            recording_shortcut: false,
            shortcut_conflict_notice: None,
            error_message: None,
        }
    }

    pub fn from_existing(item: &SoundItem) -> Self {
        Self {
            editing_id: Some(item.id.clone()),
            title: item.title.clone(),
            audio_path: Some(item.audio_path.clone()),
            image_path: item.image_path.clone(),
            duration_secs: item.duration_secs,
            volume_trim: item.volume_trim,
            shortcut: item.shortcut.clone(),
            shortcut_input: item.shortcut.clone().unwrap_or_default(),
            recording_shortcut: false,
            shortcut_conflict_notice: None,
            error_message: None,
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
            render_sound_editor_modal(ctx, draft, config, &mut outcome);
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

fn render_sound_editor_modal(
    ctx: &egui::Context,
    draft: &mut SoundModalDraft,
    config: &AppConfig,
    outcome: &mut ModalOutcome,
) {
    let title = if draft.editing_id.is_some() {
        "Edit Sound Properties"
    } else {
        "Add New Sound"
    };

    let mut open = true;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .frame(themed_modal_frame(ctx, &config.theme))
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(455.0, 325.0))
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
                            .desired_width(295.0),
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
                        if ui.button("Browse Audio...").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
                                .pick_file()
                        {
                            if draft.title.trim().is_empty()
                                && let Some(stem) = path.file_stem()
                            {
                                draft.title = stem.to_string_lossy().to_string();
                            }
                            match AudioEngine::probe_audio_duration(&path) {
                                Ok(secs) => {
                                    draft.duration_secs = secs;
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

                    ui.label("Cover Image:");
                    ui.horizontal(|ui| {
                        let img_label = draft
                            .image_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "Optional (.png / .jpg / .webp / .bmp)".to_string());
                        ui.add(
                            egui::Label::new(egui::RichText::new(img_label).weak()).truncate(),
                        );
                        if ui.button("Browse Image...").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("Image Files", &["png", "jpg", "jpeg", "webp", "bmp"])
                                .pick_file()
                        {
                            draft.image_path = Some(path);
                        }
                        if draft.image_path.is_some() && ui.button("Clear").clicked() {
                            draft.image_path = None;
                        }
                    });
                    ui.end_row();

                    ui.label("Sound Trim:");
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

                    ui.label("Shortcut:");
                    let mut sc_err: Option<String> = None;
                    let shortcut_changed = render_shortcut_picker(
                        ctx,
                        ui,
                        egui::Id::new("sound_editor_shortcut"),
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
                    let mins = (draft.duration_secs.round() as u32) / 60;
                    let secs = (draft.duration_secs.round() as u32) % 60;
                    ui.label(format!("{mins:02}:{secs:02}"));
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
                    let can_save = draft.audio_path.is_some() && shortcut_valid;
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

    if !open {
        outcome.close_modal = true;
    }
}

fn render_shortcut_picker(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    id: egui::Id,
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

    ui.horizontal(|ui| {
        if *recording {
            let coral = Color32::from_rgb(235, 110, 60);
            ui.colored_label(coral, egui::RichText::new("Press keys... (Esc cancels)").strong());
            if ui.button("Cancel").clicked() {
                *recording = false;
            }
        } else {
            let resp = ui.add(
                egui::TextEdit::singleline(shortcut_input)
                    .id(id)
                    .desired_width(136.0)
                    .hint_text("Ctrl+1 / F13 / Numpad1"),
            );
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
            let has_shortcut = !shortcut_input.trim().is_empty() || shortcut_value.is_some();
            if ui
                .add_enabled(has_shortcut, egui::Button::new("X"))
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
    egui::Window::new("WaweFX Settings")
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
    egui::Window::new("Audio Engine Status & Alerts")
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

    egui::Window::new("Android / Mobile Companion Sync")
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
