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
#[cfg(any(not(target_os = "macos"), test))]
use image::imageops::FilterType;
use uuid::Uuid;

/// Embedded WaweFX brand logo PNG bytes compiled directly into the standalone binary.
#[cfg(any(not(target_os = "macos"), test))]
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
        let path = sound.resolved_image_path()?;

        if let Some((cached_path, _)) = self.textures.get(&sound.id) {
            if *cached_path == path {
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
                    path,
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
/// application window titlebar and Windows Taskbar.
#[cfg(any(not(target_os = "macos"), test))]
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

/// Root eframe application state for WaweFX.
pub struct WaweFxApp {
    pub config: AppConfig,
    pub audio: AudioEngine,
    pub sync: SyncServerHandle,
    pub thumbnails: ThumbnailLoader,
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

        Self {
            config,
            audio,
            sync,
            thumbnails: ThumbnailLoader::new(),
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
        let removed_assets = self
            .config
            .sounds
            .iter()
            .find(|s| s.id == sound_id)
            .map(|s| (s.audio_path.clone(), s.image_path.clone()));

        self.audio.invalidate_cached_sound(sound_id);
        self.thumbnails.invalidate(sound_id);
        self.sync.invalidate_thumbnail(sound_id);
        self.config.sounds.retain(|s| s.id != sound_id);
        if self.config.selected_sound_id.as_deref() == Some(sound_id) {
            self.config.selected_sound_id = self.config.sounds.first().map(|s| s.id.clone());
        }

        if let Some((audio_path, image_path)) = removed_assets {
            self.config.cleanup_managed_path(&audio_path);
            if let Some(img) = image_path {
                self.config.cleanup_managed_path(&img);
            }
        }
    }

    fn apply_saved_sound(&mut self, draft: SoundModalDraft) {
        let Some(selected_audio) = draft.audio_path else {
            return;
        };
        let audio_path = match AppConfig::import_sound_file(&selected_audio) {
            Ok(rel) => rel,
            Err(err) => {
                self.last_error = Some(format!("Failed to copy sound into WaweFX data folder: {err:#}"));
                return;
            }
        };
        let image_path = match draft.image_path {
            Some(ref selected_img) => match AppConfig::import_cover_file(selected_img) {
                Ok(rel) => Some(rel),
                Err(err) => {
                    self.last_error = Some(format!("Failed to copy cover image into WaweFX data folder: {err:#}"));
                    None
                }
            },
            None => None,
        };

        let title = if draft.title.trim().is_empty() {
            selected_audio
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
            let mut old_paths: Option<(PathBuf, Option<PathBuf>)> = None;
            if let Some(existing) = self.config.sounds.iter_mut().find(|s| s.id == edit_id) {
                let audio_changed = existing.audio_path != audio_path;
                let image_changed = existing.image_path != image_path;
                old_paths = Some((
                    if audio_changed { existing.audio_path.clone() } else { PathBuf::new() },
                    if image_changed { existing.image_path.clone() } else { None },
                ));
                existing.title = title;
                existing.audio_path = audio_path;
                existing.image_path = image_path;
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
            if let Some((old_audio, old_img)) = old_paths {
                self.config.cleanup_managed_path(&old_audio);
                if let Some(img) = old_img {
                    self.config.cleanup_managed_path(&img);
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
                image_path,
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
        modals::apply_theme_visuals(ctx, &self.config.theme);
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
                DesktopSyncNotice::ReorderSounds { from, to } => {
                    if from < self.config.sounds.len()
                        && to < self.config.sounds.len()
                        && from != to
                    {
                        let item = self.config.sounds.remove(from);
                        self.config.sounds.insert(to, item);
                    }
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
                toolbar::render_toolbar(ui, &mut self.config, &self.audio, show_alert_badge)
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
                        match AppConfig::import_sound_file(&path) {
                            Ok(managed_audio) => {
                                let item = SoundItem::new(title, managed_audio, None, duration_secs);
                                self.config.selected_sound_id = Some(item.id.clone());
                                self.config.sounds.push(item);
                                self.last_error = None;
                                changed = true;
                            }
                            Err(err) => {
                                self.last_error = Some(format!(
                                    "Failed to copy dropped audio '{}' into WaweFX data folder: {err:#}",
                                    path.display()
                                ));
                            }
                        }
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
                    match AppConfig::import_cover_file(&path) {
                        Ok(managed_cover) => {
                            let old_img = if let Some(sound) =
                                self.config.sounds.iter_mut().find(|s| s.id == pad_id)
                            {
                                let prev = sound.image_path.replace(managed_cover.clone());
                                if prev.as_ref() == Some(&managed_cover) {
                                    None
                                } else {
                                    prev
                                }
                            } else {
                                None
                            };
                            if let Some(prev_path) = old_img {
                                self.config.cleanup_managed_path(&prev_path);
                            }
                            self.thumbnails.invalidate(&pad_id);
                            self.sync.invalidate_thumbnail(&pad_id);
                            self.last_error = None;
                            changed = true;
                        }
                        Err(err) => {
                            self.last_error = Some(format!(
                                "Failed to copy dropped cover '{}' into WaweFX data folder: {err:#}",
                                path.display()
                            ));
                        }
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
    use std::fs;
    use std::io::Cursor;
    use std::path::Path;

    use image::codecs::ico::{IcoDecoder, IcoEncoder, IcoFrame};
    use image::codecs::png::PngEncoder;
    use image::imageops::FilterType;
    use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, Rgba, RgbaImage};

    use super::{LOGO_PNG_BYTES, build_window_icon};

    fn rounded_rect_coverage(px: u32, py: u32, size: u32, radius: f32) -> f32 {
        let half = size as f32 * 0.5;
        let dx = (px as f32 + 0.5 - half).abs() - (half - radius);
        let dy = (py as f32 + 0.5 - half).abs() - (half - radius);
        let dist = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
        (0.5 - dist / 1.2).clamp(0.0, 1.0)
    }

    fn write_rgba8_png(path: &Path, rgba: &RgbaImage) {
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(rgba.as_raw(), rgba.width(), rgba.height(), ExtendedColorType::Rgba8)
            .expect("encode Rgba8 PNG");
        fs::write(path, &out).expect("write PNG file");
    }

    #[test]
    fn decodes_embedded_logo_and_platform_icon_assets_exist() {
        let icon = build_window_icon();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert_eq!(&icon.rgba[0..4], &[0, 0, 0, 0]);
        let center_off = (128 * 256 + 128) * 4;
        assert_eq!(icon.rgba[center_off + 3], 255);
        assert!(LOGO_PNG_BYTES.starts_with(b"\x89PNG\r\n\x1a\n"));

        let logo_img = image::load_from_memory(LOGO_PNG_BYTES).expect("decode logo.png");
        assert_eq!(logo_img.color(), image::ColorType::Rgba8);
        let logo_rgba = logo_img.to_rgba8();
        assert_eq!(logo_rgba.get_pixel(0, 0).0[3], 0);
        assert_eq!(
            logo_rgba
                .get_pixel(logo_rgba.width() / 2, logo_rgba.height() / 2)
                .0[3],
            255
        );

        assert!(Path::new("assets/icon.ico").is_file());
        assert!(Path::new("assets/icon.icns").is_file());

        let ico_bytes = fs::read("assets/icon.ico").expect("read assets/icon.ico");
        let ico_dec = IcoDecoder::new(Cursor::new(&ico_bytes)).expect("decode assets/icon.ico");
        let (iw, ih) = ico_dec.dimensions();
        let mut ico_buf = vec![0u8; ico_dec.total_bytes() as usize];
        let color_type = ico_dec.color_type();
        ico_dec.read_image(&mut ico_buf).expect("read ICO pixels");
        assert_eq!(color_type, image::ColorType::Rgba8);
        assert_eq!(ico_buf[3], 0, "ICO top-left corner must be transparent");
        let ico_center = (((ih / 2) * iw + (iw / 2)) * 4) as usize;
        assert_eq!(ico_buf[ico_center + 3], 255);
    }

    #[test]
    #[ignore]
    fn regenerate_rounded_icons_and_assets() {
        let raw = fs::read("assets/logo.png").expect("read assets/logo.png");
        let orig = image::load_from_memory(&raw).expect("decode assets/logo.png").to_rgba8();

        // Crop existing transparent outer padding idempotently so re-running never shrinks further
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (orig.width(), orig.height(), 0u32, 0u32);
        for (x, y, p) in orig.enumerate_pixels() {
            if p.0[3] > 8 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let crop_w = (max_x - min_x + 1).max(1);
        let crop_h = (max_y - min_y + 1).max(1);
        let cropped = image::imageops::crop_imm(&orig, min_x, min_y, crop_w, crop_h).to_image();

        let canvas_size: u32 = 1024;
        let pad: u32 = 82; // ~8% transparent outer padding for macOS Dock & Windows 11 proportions
        let inner_size = canvas_size - pad * 2; // 860x860 rounded square
        let radius = inner_size as f32 * 0.22;

        let inner_rgba = DynamicImage::ImageRgba8(cropped)
            .resize_exact(inner_size, inner_size, FilterType::Lanczos3)
            .to_rgba8();

        let mut canvas = RgbaImage::from_pixel(canvas_size, canvas_size, Rgba([0, 0, 0, 0]));
        for y in 0..inner_size {
            for x in 0..inner_size {
                let cov = rounded_rect_coverage(x, y, inner_size, radius);
                if cov <= 0.0 {
                    continue;
                }
                let src = inner_rgba.get_pixel(x, y).0;
                let alpha = ((src[3] as f32) * cov).round().clamp(0.0, 255.0) as u8;
                if alpha > 0 {
                    canvas.put_pixel(pad + x, pad + y, Rgba([src[0], src[1], src[2], alpha]));
                }
            }
        }

        write_rgba8_png(Path::new("assets/logo.png"), &canvas);

        // Regenerate multi-resolution 32-bit RGBA assets/icon.ico via image's IcoEncoder
        let rounded_dyn = DynamicImage::ImageRgba8(canvas);
        let ico_sizes: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
        let mut frames: Vec<IcoFrame<'static>> = Vec::with_capacity(ico_sizes.len());
        for &size in ico_sizes {
            let resized = rounded_dyn
                .resize_exact(size, size, FilterType::Lanczos3)
                .to_rgba8();
            let frame = IcoFrame::as_png(resized.as_raw(), size, size, ExtendedColorType::Rgba8)
                .expect("encode RGBA ICO frame");
            frames.push(frame);
        }
        let mut ico_bytes = Vec::new();
        IcoEncoder::new(&mut ico_bytes)
            .encode_images(&frames)
            .expect("encode multi-size ICO");
        fs::write("assets/icon.ico", &ico_bytes).expect("write assets/icon.ico");

        // Regenerate Apple assets/icon.icns with full 32-bit RGBA PNG chunks
        let icns_specs: &[(&[u8; 4], u32)] = &[
            (b"icp4", 16),
            (b"icp5", 32),
            (b"icp6", 64),
            (b"ic07", 128),
            (b"ic08", 256),
            (b"ic09", 512),
            (b"ic10", 1024),
        ];
        let mut chunks: Vec<(&[u8; 4], Vec<u8>)> = Vec::with_capacity(icns_specs.len());
        let mut total_len: u32 = 8;
        for &(tag, size) in icns_specs {
            let resized = rounded_dyn
                .resize_exact(size, size, FilterType::Lanczos3)
                .to_rgba8();
            let mut png_buf = Vec::new();
            PngEncoder::new(&mut png_buf)
                .write_image(resized.as_raw(), size, size, ExtendedColorType::Rgba8)
                .expect("encode RGBA ICNS chunk");
            total_len += 8 + png_buf.len() as u32;
            chunks.push((tag, png_buf));
        }
        let mut icns = Vec::with_capacity(total_len as usize);
        icns.extend_from_slice(b"icns");
        icns.extend_from_slice(&total_len.to_be_bytes());
        for (tag, png_buf) in chunks {
            icns.extend_from_slice(tag);
            icns.extend_from_slice(&((8 + png_buf.len()) as u32).to_be_bytes());
            icns.extend_from_slice(&png_buf);
        }
        fs::write("assets/icon.icns", &icns).expect("write assets/icon.icns");
    }

    #[test]
    #[ignore]
    fn generate_macos_iconset_files() {
        let img = image::load_from_memory(LOGO_PNG_BYTES).expect("decode embedded logo.png");
        let iconset_dir = Path::new("AppIcon.iconset");
        let _ = fs::remove_dir_all(iconset_dir);
        fs::create_dir_all(iconset_dir).expect("create AppIcon.iconset");

        let specs: &[(&str, u32)] = &[
            ("icon_16x16.png", 16),
            ("icon_16x16@2x.png", 32),
            ("icon_32x32.png", 32),
            ("icon_32x32@2x.png", 64),
            ("icon_128x128.png", 128),
            ("icon_128x128@2x.png", 256),
            ("icon_256x256.png", 256),
            ("icon_256x256@2x.png", 512),
            ("icon_512x512.png", 512),
            ("icon_512x512@2x.png", 1024),
        ];

        for &(filename, size) in specs {
            let resized = img.resize_exact(size, size, FilterType::Lanczos3).to_rgba8();
            write_rgba8_png(&iconset_dir.join(filename), &resized);
        }
    }
}
