use std::path::PathBuf;

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use qrcode::{Color as QrColor, QrCode};

use crate::audio::{AudioDeviceList, AudioEngine, AudioEngineStatus, apo_installer};
use crate::config::{AppConfig, SoundItem, ThemePreset};

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
            error_message: None,
        }
    }
}

/// Actions requested by modal interactions on a single frame.
#[derive(Debug, Default)]
pub struct ModalOutcome {
    pub close_modal: bool,
    pub save_sound: Option<SoundModalDraft>,
    pub apply_audio_config: bool,
    pub refresh_devices: bool,
    pub open_settings: bool,
    pub config_dirty: bool,
}

/// Renders whichever modal dialog is currently active.
pub fn render_modals(
    ctx: &egui::Context,
    modal: &mut ActiveModal,
    config: &mut AppConfig,
    devices: &AudioDeviceList,
    status: &AudioEngineStatus,
    local_ip: &str,
    connected_clients: usize,
) -> ModalOutcome {
    let mut outcome = ModalOutcome::default();

    match modal {
        ActiveModal::SoundEditor(draft) => {
            render_sound_editor_modal(ctx, draft, &mut outcome);
        }
        ActiveModal::Settings => {
            render_settings_modal(ctx, config, devices, &mut outcome);
        }
        ActiveModal::Alerts => {
            render_alerts_modal(ctx, status, &mut outcome);
        }
        ActiveModal::CompanionPairing => {
            render_companion_modal(ctx, config.audio.sync_port, local_ip, connected_clients, &mut outcome);
        }
    }

    outcome
}

fn render_sound_editor_modal(
    ctx: &egui::Context,
    draft: &mut SoundModalDraft,
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
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(430.0, 265.0))
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
                            .desired_width(275.0),
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
                            .unwrap_or_else(|| "Optional (.png / .jpg / .webp)".to_string());
                        ui.add(
                            egui::Label::new(egui::RichText::new(img_label).weak()).truncate(),
                        );
                        if ui.button("Browse Image...").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("Image Files", &["png", "jpg", "jpeg", "webp"])
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

                    ui.label("Duration:");
                    let mins = (draft.duration_secs.round() as u32) / 60;
                    let secs = (draft.duration_secs.round() as u32) % 60;
                    ui.label(format!("{mins:02}:{secs:02}"));
                    ui.end_row();
                });

            if let Some(err) = &draft.error_message {
                ui.add_space(6.0);
                ui.colored_label(Color32::from_rgb(220, 70, 70), err);
            }

            ui.add_space(12.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    outcome.close_modal = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_save = draft.audio_path.is_some();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save Sound"))
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

fn render_settings_modal(
    ctx: &egui::Context,
    config: &mut AppConfig,
    devices: &AudioDeviceList,
    outcome: &mut ModalOutcome,
) {
    let mut open = true;
    egui::Window::new("WaweFX Settings")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .default_size(Vec2::new(500.0, 400.0))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // --- Section 1: Audio Routing Devices ---
                ui.heading("Audio Devices & Routing");
                ui.add_space(4.0);

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
                ui.horizontal(|ui| {
                    if ui.button("Apply & Restart Streams").clicked() {
                        outcome.apply_audio_config = true;
                        outcome.config_dirty = true;
                    }
                    if ui.button("Refresh Devices").clicked() {
                        outcome.refresh_devices = true;
                    }
                });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);

                // --- Section 2: Color Themes (3 Presets + Custom RGB Pickers) ---
                ui.heading("Theme & Colors");
                ui.add_space(4.0);

                ui.horizontal_wrapped(|ui| {
                    for preset in ThemePreset::BUILTIN_PRESETS {
                        let active = config.theme.preset == preset;
                        if ui.selectable_label(active, preset.label()).clicked() {
                            config.theme.apply_preset(preset);
                            outcome.config_dirty = true;
                        }
                    }
                    if config.theme.preset == ThemePreset::Custom {
                        ui.label(egui::RichText::new("• Custom").strong());
                    }
                });

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

                        color_changed |= color_row(ui, "Accent / Coral", &mut config.theme.accent_color);
                        color_changed |= color_row(ui, "Pad Surface", &mut config.theme.pad_card_bg);
                        ui.end_row();

                        color_changed |= color_row(ui, "Pad Hover", &mut config.theme.pad_card_hover);
                        color_changed |= color_row(ui, "Pad Text", &mut config.theme.pad_text);
                        ui.end_row();
                    });

                if color_changed {
                    config.theme.mark_custom_if_changed();
                    outcome.config_dirty = true;
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Reset to Classic WaweFX").clicked() {
                        config.theme.apply_preset(ThemePreset::ClassicLilac);
                        outcome.config_dirty = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Done").clicked() {
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
    status: &AudioEngineStatus,
    outcome: &mut ModalOutcome,
) {
    let mut open = true;
    egui::Window::new("Audio Engine Status & Alerts")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(470.0, 295.0))
        .show(ctx, |ui| {
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

            ui.add_space(8.0);
            ui.separator();

            if status.warnings.is_empty() {
                ui.colored_label(
                    Color32::from_rgb(46, 160, 67),
                    "All active audio streams are running normally.",
                );
            } else {
                ui.label(egui::RichText::new("Warnings:").strong());
                for warning in &status.warnings {
                    ui.colored_label(Color32::from_rgb(235, 110, 60), format!("• {warning}"));
                }
            }

            ui.add_space(8.0);
            ui.small(apo_installer::query_apo_status().notes);

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Open Audio Settings").clicked() {
                    outcome.open_settings = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        outcome.close_modal = true;
                    }
                });
            });
        });

    if !open {
        outcome.close_modal = true;
    }
}

fn render_companion_modal(
    ctx: &egui::Context,
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
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(Vec2::new(340.0, 360.0))
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

#[cfg(test)]
mod tests {
    use super::virtual_mic_priority;

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
}
