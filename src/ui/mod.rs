pub mod grid;
pub mod modals;
pub mod toolbar;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use image::ImageReader;
use image::imageops::FilterType;
use uuid::Uuid;

/// Embedded WaweFX brand logo PNG bytes compiled directly into the standalone binary.
pub const LOGO_PNG_BYTES: &[u8] = include_bytes!("../../assets/logo.png");

use crate::audio::{AudioDeviceList, AudioEngine};
use crate::config::{AppConfig, SoundItem, rgb_to_color32};
use crate::hotkeys::{HotkeyAction, HotkeyService};
use crate::sync::{DesktopSyncNotice, SyncServerHandle};
use crate::ui::modals::{ActiveModal, DriverSetupState, SoundModalDraft};

struct DecodeRequest {
    sound_id: String,
    path: PathBuf,
    ctx: egui::Context,
}

struct DecodedThumbnail {
    sound_id: String,
    path: PathBuf,
    image: ColorImage,
}

/// Non-blocking thumbnail loader that decodes and downscales images on a worker thread.
pub struct ThumbnailLoader {
    req_tx: Sender<DecodeRequest>,
    res_rx: Receiver<DecodedThumbnail>,
    textures: HashMap<String, (PathBuf, TextureHandle)>,
    pending: HashSet<(String, PathBuf)>,
    failed: HashSet<(String, PathBuf)>,
}

impl ThumbnailLoader {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<DecodeRequest>();
        let (res_tx, res_rx) = mpsc::channel::<DecodedThumbnail>();

        thread::Builder::new()
            .name("wawefx-thumbnail-loader".to_string())
            .spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    if let Some(color_img) = decode_thumbnail_image(&req.path) {
                        let ctx = req.ctx;
                        if res_tx
                            .send(DecodedThumbnail {
                                sound_id: req.sound_id,
                                path: req.path,
                                image: color_img,
                            })
                            .is_ok()
                        {
                            ctx.request_repaint();
                        }
                    }
                }
            })
            .expect("failed to spawn thumbnail decoder thread");

        Self {
            req_tx,
            res_rx,
            textures: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// Drains completed background image decodes and uploads them as GPU textures.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(item) = self.res_rx.try_recv() {
            self.pending.remove(&(item.sound_id.clone(), item.path.clone()));
            let handle = ctx.load_texture(
                format!("pad_thumb_{}", item.sound_id),
                item.image,
                TextureOptions::LINEAR,
            );
            self.textures.insert(item.sound_id, (item.path, handle));
        }
    }

    /// Returns a cached texture if ready, or schedules an async background decode if missing.
    pub fn texture_for(&mut self, ctx: &egui::Context, sound: &SoundItem) -> Option<&TextureHandle> {
        let path = sound.image_path.as_ref()?;

        if let Some((cached_path, _)) = self.textures.get(&sound.id) {
            if cached_path == path {
                return self.textures.get(&sound.id).map(|(_, h)| h);
            }
            self.textures.remove(&sound.id);
        }

        let key = (sound.id.clone(), path.clone());
        if !self.pending.contains(&key) && !self.failed.contains(&key) {
            if path.exists() {
                self.pending.insert(key);
                let _ = self.req_tx.send(DecodeRequest {
                    sound_id: sound.id.clone(),
                    path: path.clone(),
                    ctx: ctx.clone(),
                });
            } else {
                self.failed.insert(key);
            }
        }

        None
    }

    /// Clears cached texture/failure state when a sound's image changes or the sound is removed.
    pub fn invalidate(&mut self, sound_id: &str) {
        self.textures.remove(sound_id);
        self.pending.retain(|(id, _)| id != sound_id);
        self.failed.retain(|(id, _)| id != sound_id);
    }
}

impl Default for ThumbnailLoader {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_thumbnail_image(path: &Path) -> Option<ColorImage> {
    let reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let decoded = reader.decode().ok()?;
    let thumb = decoded.thumbnail(256, 256).to_rgba8();
    let size = [thumb.width() as usize, thumb.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, thumb.as_raw()))
}

/// Decodes the embedded `assets/logo.png` into a 256x256 RGBA [`egui::IconData`] for the
/// application window titlebar, Windows Taskbar, and macOS Dock.
pub fn build_window_icon() -> egui::IconData {
    if let Ok(decoded) = image::load_from_memory(LOGO_PNG_BYTES) {
        let resized = decoded
            .resize_exact(256, 256, FilterType::Lanczos3)
            .to_rgba8();
        return egui::IconData {
            rgba: resized.into_raw(),
            width: 256,
            height: 256,
        };
    }

    let size = 32usize;
    let rgba = vec![0xF4u8; size * size * 4];
    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}

/// Decodes the embedded `assets/logo.png` into a crisp HiDPI GPU texture for the top-left header.
fn load_header_logo_texture(ctx: &egui::Context) -> Option<TextureHandle> {
    let decoded = image::load_from_memory(LOGO_PNG_BYTES).ok()?;
    let resized = decoded
        .resize_exact(128, 128, FilterType::Lanczos3)
        .to_rgba8();
    let size = [resized.width() as usize, resized.height() as usize];
    let image = ColorImage::from_rgba_unmultiplied(size, resized.as_raw());
    Some(ctx.load_texture("wawefx_header_logo", image, TextureOptions::LINEAR))
}

/// Root eframe application state for WaweFX.
pub struct WaweFxApp {
    pub config: AppConfig,
    pub audio: AudioEngine,
    pub sync: SyncServerHandle,
    pub thumbnails: ThumbnailLoader,
    pub logo_texture: Option<TextureHandle>,
    pub active_modal: Option<ActiveModal>,
    pub cached_devices: AudioDeviceList,
    pub driver_setup: DriverSetupState,
    pub hotkeys: HotkeyService,
    pub local_ip: String,
    pub last_error: Option<String>,
    last_playback_state: (bool, Option<String>),
}

impl WaweFxApp {
    pub fn new(
        ctx: &egui::Context,
        config: AppConfig,
        audio: AudioEngine,
        sync: SyncServerHandle,
    ) -> Self {
        let cached_devices = audio.available_devices();
        let local_ip = sync.local_ip();
        let last_playback_state = (audio.is_playing(), audio.active_sound_id());
        let mut hotkeys = HotkeyService::new(ctx);
        hotkeys.sync_bindings(&config);
        let logo_texture = load_header_logo_texture(ctx);

        Self {
            config,
            audio,
            sync,
            thumbnails: ThumbnailLoader::new(),
            logo_texture,
            active_modal: None,
            cached_devices,
            driver_setup: DriverSetupState::default(),
            hotkeys,
            local_ip,
            last_error: None,
            last_playback_state,
        }
    }

    fn play_sound_by_id(&mut self, sound_id: &str) {
        if let Some(sound) = self.config.sounds.iter().find(|s| s.id == sound_id).cloned() {
            match self.audio.play_item(&sound) {
                Ok(()) => self.last_error = None,
                Err(err) => {
                    self.last_error = Some(format!("Playback failed for '{}': {err}", sound.title));
                }
            }
        }
    }

    fn remove_sound_by_id(&mut self, sound_id: &str) {
        if self.audio.active_sound_id().as_deref() == Some(sound_id) {
            self.audio.stop();
        }
        self.audio.invalidate_cached_sound(sound_id);
        self.thumbnails.invalidate(sound_id);
        self.sync.invalidate_thumbnail(sound_id);
        self.config.sounds.retain(|s| s.id != sound_id);
        if self.config.selected_sound_id.as_deref() == Some(sound_id) {
            self.config.selected_sound_id = self.config.sounds.first().map(|s| s.id.clone());
        }
    }

    fn apply_saved_sound(&mut self, draft: SoundModalDraft) {
        let Some(audio_path) = draft.audio_path else {
            return;
        };
        let title = if draft.title.trim().is_empty() {
            audio_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Untitled Sound".to_string())
        } else {
            draft.title.trim().to_string()
        };

        let normalized_shortcut = draft
            .shortcut
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| crate::hotkeys::normalize_shortcut(s).unwrap_or_else(|_| s.to_string()));

        if let Some(edit_id) = draft.editing_id {
            if let Some(ref sc) = normalized_shortcut {
                self.config
                    .unbind_shortcut_conflicts(sc, Some(&edit_id), false);
            }
            if let Some(existing) = self.config.sounds.iter_mut().find(|s| s.id == edit_id) {
                let audio_changed = existing.audio_path != audio_path;
                let image_changed = existing.image_path != draft.image_path;
                existing.title = title;
                existing.audio_path = audio_path;
                existing.image_path = draft.image_path;
                existing.duration_secs = draft.duration_secs;
                existing.volume_trim = draft.volume_trim;
                existing.shortcut = normalized_shortcut;
                if audio_changed {
                    self.audio.invalidate_cached_sound(&edit_id);
                }
                if image_changed {
                    self.thumbnails.invalidate(&edit_id);
                    self.sync.invalidate_thumbnail(&edit_id);
                }
            }
        } else {
            let new_id = Uuid::new_v4().to_string();
            if let Some(ref sc) = normalized_shortcut {
                self.config
                    .unbind_shortcut_conflicts(sc, Some(&new_id), false);
            }
            let new_item = SoundItem {
                id: new_id.clone(),
                title,
                audio_path,
                image_path: draft.image_path,
                duration_secs: draft.duration_secs,
                volume_trim: draft.volume_trim,
                shortcut: normalized_shortcut,
            };
            self.config.sounds.push(new_item);
            self.config.selected_sound_id = Some(new_id);
        }
    }
}

impl eframe::App for WaweFxApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sync.register_egui_ctx(ctx.clone());
        self.thumbnails.poll(ctx);

        let mut config_dirty = false;
        let mut sync_dirty = false;

        self.hotkeys.sync_bindings(&self.config);
        for hk_action in self.hotkeys.poll_triggered() {
            match hk_action {
                HotkeyAction::PlaySound(id) => {
                    self.play_sound_by_id(&id);
                    self.config.selected_sound_id = Some(id);
                    sync_dirty = true;
                    ctx.request_repaint();
                }
                HotkeyAction::StopAll => {
                    self.audio.stop();
                    sync_dirty = true;
                    ctx.request_repaint();
                }
            }
        }

        for notice in self.sync.poll_desktop_notices() {
            match notice {
                DesktopSyncNotice::SelectSound(id) => {
                    self.config.selected_sound_id = Some(id);
                }
                DesktopSyncNotice::SetPitch(speed) => {
                    self.config.audio.playback_speed = speed;
                }
                DesktopSyncNotice::SetMicEnabled(enabled) => {
                    self.config.audio.mic_stream_enabled = enabled;
                }
                DesktopSyncNotice::SetHeadphonesEnabled(enabled) => {
                    self.config.audio.headphones_stream_enabled = enabled;
                }
            }
            config_dirty = true;
        }

        let status = self.audio.status_snapshot();
        let show_alert_badge = status.has_warning
            || !modals::has_virtual_audio_device(&self.cached_devices)
            || !self.hotkeys.status().warnings.is_empty();
        let bg_toolbar = rgb_to_color32(self.config.theme.bg_toolbar);
        let bg_main = rgb_to_color32(self.config.theme.bg_main);

        // Top Pixel-Accurate Toolbar
        let tb_action = egui::TopBottomPanel::top("wawefx_toolbar")
            .exact_height(toolbar::TOOLBAR_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(bg_toolbar))
            .show(ctx, |ui| {
                toolbar::render_toolbar(
                    ui,
                    &mut self.config,
                    &self.audio,
                    self.logo_texture.as_ref(),
                    show_alert_badge,
                )
            })
            .inner;

        if tb_action.config_dirty {
            config_dirty = true;
            sync_dirty = true;
        }

        if tb_action.add_sound {
            self.active_modal = Some(ActiveModal::SoundEditor(SoundModalDraft::new_add()));
        }
        if tb_action.remove_selected
            && let Some(selected_id) = self.config.selected_sound_id.clone()
        {
            self.remove_sound_by_id(&selected_id);
            config_dirty = true;
            sync_dirty = true;
        }
        if tb_action.edit_selected
            && let Some(selected_id) = self.config.selected_sound_id.as_deref()
            && let Some(item) = self.config.sounds.iter().find(|s| s.id == selected_id)
        {
            self.active_modal = Some(ActiveModal::SoundEditor(SoundModalDraft::from_existing(item)));
        }
        if tb_action.play_selected
            && let Some(selected_id) = self.config.selected_sound_id.clone()
        {
            self.play_sound_by_id(&selected_id);
            sync_dirty = true;
        }
        if tb_action.stop_playback {
            self.audio.stop();
            sync_dirty = true;
        }
        if tb_action.rewind {
            self.audio.rewind();
            sync_dirty = true;
        }
        if tb_action.fast_forward {
            self.audio.fast_forward();
            sync_dirty = true;
        }
        if tb_action.seeked {
            sync_dirty = true;
        }
        if tb_action.open_alerts {
            self.cached_devices = self.audio.available_devices();
            self.active_modal = Some(ActiveModal::Alerts);
        }
        if tb_action.open_companion {
            self.local_ip = self.sync.local_ip();
            self.active_modal = Some(ActiveModal::CompanionPairing);
        }
        if tb_action.open_settings {
            self.cached_devices = self.audio.available_devices();
            self.active_modal = Some(ActiveModal::Settings);
        }

        // Right-Docked Vertical Pitch / Speed Slider
        let pitch_changed = egui::SidePanel::right("wawefx_pitch_panel")
            .resizable(false)
            .exact_width(44.0)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(bg_main))
            .show(ctx, |ui| {
                toolbar::render_vertical_pitch_panel(ui, &mut self.config, &self.audio)
            })
            .inner;
        if pitch_changed {
            config_dirty = true;
            sync_dirty = true;
        }

        let accent = rgb_to_color32(self.config.theme.accent_color);
        let (hovered_files, dropped_files, drop_pointer_pos) = ctx.input(|i| {
            (
                i.raw.hovered_files.clone(),
                i.raw.dropped_files.clone(),
                i.pointer.latest_pos().or(i.pointer.interact_pos()),
            )
        });
        let hovered_image_drop = hovered_files.iter().any(|f| {
            f.path.as_deref().is_some_and(is_image_file) || f.mime.starts_with("image/")
        });
        let hovered_audio_drop = hovered_files.iter().any(|f| {
            f.path.as_deref().is_some_and(is_audio_file) || f.mime.starts_with("audio/")
        });

        // Main Soundboard Canvas / Grid
        let inner_margin = if self.config.sounds.is_empty()
            && self.last_error.is_none()
            && hovered_files.is_empty()
        {
            0
        } else {
            16
        };
        let grid_action = egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(bg_main)
                    .inner_margin(egui::Margin::same(inner_margin)),
            )
            .show(ctx, |ui| {
                if !hovered_files.is_empty() {
                    let hint = if hovered_image_drop && !hovered_audio_drop {
                        "Drop image onto a Sound Pad card to set its cover"
                    } else if hovered_audio_drop && !hovered_image_drop {
                        "Drop audio file anywhere to add a new Sound Pad"
                    } else {
                        "Drop audio files to add pads, or drop an image onto a pad card"
                    };
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgba_unmultiplied(
                            accent.r(),
                            accent.g(),
                            accent.b(),
                            28,
                        ))
                        .stroke(egui::Stroke::new(1.5_f32, accent))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.colored_label(
                                accent,
                                egui::RichText::new(hint).strong().size(13.0),
                            );
                        });
                    ui.add_space(8.0);
                }

                if let Some(err) = self.last_error.clone() {
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(200, 45, 45), err);
                        if ui.small_button("Dismiss").clicked() {
                            self.last_error = None;
                        }
                    });
                    ui.add_space(8.0);
                }

                grid::render_sound_grid(
                    ui,
                    &mut self.config,
                    &self.audio,
                    &mut self.thumbnails,
                    hovered_image_drop,
                )
            })
            .inner;

        if grid_action.config_dirty {
            config_dirty = true;
            sync_dirty = true;
        }

        if !dropped_files.is_empty()
            && self.handle_dropped_files(&dropped_files, drop_pointer_pos, &grid_action.pad_rects)
        {
            config_dirty = true;
            sync_dirty = true;
        }

        if let Some(play_id) = grid_action.play_sound_id {
            self.play_sound_by_id(&play_id);
            sync_dirty = true;
        }
        if grid_action.stop_playback {
            self.audio.stop();
            sync_dirty = true;
        }
        if let Some(edit_id) = grid_action.edit_sound_id
            && let Some(item) = self.config.sounds.iter().find(|s| s.id == edit_id)
        {
            self.active_modal = Some(ActiveModal::SoundEditor(SoundModalDraft::from_existing(item)));
        }
        if let Some(assign_id) = grid_action.assign_shortcut_id
            && let Some(item) = self.config.sounds.iter().find(|s| s.id == assign_id)
        {
            self.active_modal = Some(ActiveModal::SoundEditor(
                SoundModalDraft::from_existing_recording_shortcut(item),
            ));
        }
        if let Some(clear_id) = grid_action.clear_shortcut_id
            && let Some(item) = self.config.sounds.iter_mut().find(|s| s.id == clear_id)
        {
            item.shortcut = None;
            config_dirty = true;
            sync_dirty = true;
        }
        if let Some(remove_id) = grid_action.remove_sound_id {
            self.remove_sound_by_id(&remove_id);
            config_dirty = true;
            sync_dirty = true;
        }

        // Render active modal window
        if let Some(mut modal) = self.active_modal.take() {
            let bound_port = self.sync.bound_port();
            let connected_clients = self.sync.connected_clients();
            let mut modal_cfg = self.config.clone();
            modal_cfg.audio.sync_port = bound_port;

            let outcome = modals::render_modals(
                ctx,
                &mut modal,
                &mut modal_cfg,
                &self.cached_devices,
                &status,
                self.hotkeys.status(),
                &self.driver_setup,
                &self.local_ip,
                connected_clients,
            );
            if outcome.config_dirty {
                modal_cfg.audio.sync_port = self.config.audio.sync_port;
                self.config = modal_cfg;
                config_dirty = true;
                sync_dirty = true;
            }
            if outcome.refresh_hotkey_permissions {
                self.hotkeys.refresh_permissions();
                self.hotkeys.sync_bindings(&self.config);
            }

            if outcome.refresh_and_apply_devices {
                self.cached_devices = self.audio.available_devices();
                if self.config.audio.virtual_mic_output_device.is_none()
                    && let Some(preferred) =
                        modals::find_preferred_virtual_mic_output(&self.cached_devices)
                {
                    self.config.audio.virtual_mic_output_device = Some(preferred);
                    config_dirty = true;
                }
                self.audio.apply_routing_config(&self.config.audio);
                sync_dirty = true;
            } else if outcome.apply_audio_config {
                self.audio.apply_routing_config(&self.config.audio);
                sync_dirty = true;
            }
            if let Some(draft) = outcome.save_sound {
                self.apply_saved_sound(draft);
                config_dirty = true;
                sync_dirty = true;
            }
            if outcome.open_settings {
                self.cached_devices = self.audio.available_devices();
                self.active_modal = Some(ActiveModal::Settings);
            } else if !outcome.close_modal {
                self.active_modal = Some(modal);
            }
        }

        if config_dirty {
            self.hotkeys.sync_bindings(&self.config);
            if let Err(err) = self.config.save() {
                self.last_error = Some(format!("Failed to save config: {err:#}"));
            }
        }

        let current_playback = (self.audio.is_playing(), self.audio.active_sound_id());
        if current_playback != self.last_playback_state {
            self.last_playback_state = current_playback;
            sync_dirty = true;
        }

        if sync_dirty {
            self.sync.publish_state(&self.config, &self.audio);
        }

        // Animate the playing pad's bottom progress bar smoothly while audio is playing,
        // and refresh live callback telemetry or background driver setup progress.
        if self.audio.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.driver_setup.is_running() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if matches!(self.active_modal, Some(ActiveModal::Alerts)) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl WaweFxApp {
    /// Processes external audio/image files dropped onto the application window.
    fn handle_dropped_files(
        &mut self,
        dropped_files: &[egui::DroppedFile],
        drop_pos: Option<egui::Pos2>,
        pad_rects: &[(String, egui::Rect)],
    ) -> bool {
        let mut changed = false;

        for dropped in dropped_files {
            let Some(path) = dropped.path.clone() else {
                self.last_error = Some("Dropped item does not provide a local file path.".to_string());
                continue;
            };

            if is_audio_file(&path) {
                match AudioEngine::probe_audio_duration(&path) {
                    Ok(duration_secs) => {
                        let title = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or("New Sound")
                            .to_string();
                        let item = SoundItem::new(title, path, None, duration_secs);
                        self.config.selected_sound_id = Some(item.id.clone());
                        self.config.sounds.push(item);
                        self.last_error = None;
                        changed = true;
                    }
                    Err(err) => {
                        self.last_error = Some(format!(
                            "Failed to decode dropped audio '{}': {err}",
                            path.display()
                        ));
                    }
                }
            } else if is_image_file(&path) {
                let target_pad_id = drop_pos.and_then(|pos| {
                    pad_rects
                        .iter()
                        .find(|(_, rect)| rect.contains(pos))
                        .map(|(id, _)| id.clone())
                });

                if let Some(pad_id) = target_pad_id {
                    if let Some(sound) = self.config.sounds.iter_mut().find(|s| s.id == pad_id) {
                        sound.image_path = Some(path);
                        self.thumbnails.invalidate(&pad_id);
                        self.sync.invalidate_thumbnail(&pad_id);
                        self.last_error = None;
                        changed = true;
                    }
                } else {
                    self.last_error = Some(
                        "Drop an image directly onto a Sound Pad card to set its cover."
                            .to_string(),
                    );
                }
            } else {
                self.last_error = Some(format!(
                    "Unsupported file type '{}'. Supported: mp3, wav, ogg, flac, png, jpg, jpeg, webp.",
                    path.display()
                ));
            }
        }

        changed
    }
}

fn has_extension_in(path: &Path, allowed: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let lower = ext.to_ascii_lowercase();
            allowed.contains(&lower.as_str())
        })
}

fn is_audio_file(path: &Path) -> bool {
    has_extension_in(path, &["mp3", "wav", "ogg", "flac"])
}

fn is_image_file(path: &Path) -> bool {
    has_extension_in(path, &["png", "jpg", "jpeg", "webp"])
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{LOGO_PNG_BYTES, build_window_icon};

    #[test]
    fn decodes_embedded_logo_and_platform_icon_assets_exist() {
        let icon = build_window_icon();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(LOGO_PNG_BYTES.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(Path::new("assets/icon.ico").is_file());
        assert!(Path::new("assets/icon.icns").is_file());
    }
}
