pub mod grid;
pub mod modals;
pub mod toolbar;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageFormat, ImageReader, RgbaImage};
use uuid::Uuid;

const COVER_THUMB_MAX_DIM: u32 = 256;
const MIN_GIF_DELAY: Duration = Duration::from_millis(20);
const FALLBACK_GIF_DELAY: Duration = Duration::from_millis(50);

/// Embedded WaweFX brand logo PNG bytes compiled directly into the standalone binary.
pub const LOGO_PNG_BYTES: &[u8] = include_bytes!("../../assets/logo.png");

use crate::audio::{AudioDeviceList, AudioEngine, PREVIEW_SOUND_ID};
use crate::config::{
    AppConfig, ShortcutOwner, SoundItem, rgb_to_color32, sanitize_trim_range, step_playback_speed,
};
use crate::hotkeys::{HotkeyAction, HotkeyService};
use crate::sync::{DesktopSyncNotice, SyncServerHandle};
use crate::ui::modals::{ActiveModal, DriverSetupState, SoundModalDraft};
use crate::updater::UpdateChecker;

struct DecodeRequest {
    sound_id: String,
    path: PathBuf,
    ctx: egui::Context,
}

#[derive(Clone)]
struct DecodedGifFrame {
    image: ColorImage,
    delay: Duration,
}

#[derive(Clone)]
enum DecodedCover {
    Static(ColorImage),
    Animated {
        frames: Vec<DecodedGifFrame>,
        total_duration: Duration,
    },
}

struct GifFrame {
    handle: TextureHandle,
    delay: Duration,
}

enum CachedCover {
    Static(TextureHandle),
    Animated {
        frames: Vec<GifFrame>,
        total_duration: Duration,
    },
}

struct DecodedThumbnail {
    sound_id: String,
    path: PathBuf,
    cover: DecodedCover,
}

/// Non-blocking thumbnail loader that decodes and downscales images (and animated GIFs) on a worker thread.
pub struct ThumbnailLoader {
    req_tx: Sender<DecodeRequest>,
    res_rx: Receiver<DecodedThumbnail>,
    textures: HashMap<String, (PathBuf, CachedCover)>,
    pending: HashSet<(String, PathBuf)>,
    failed: HashSet<(String, PathBuf)>,
    generation: u64,
}

impl ThumbnailLoader {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<DecodeRequest>();
        let (res_tx, res_rx) = mpsc::channel::<DecodedThumbnail>();

        thread::Builder::new()
            .name("wawefx-thumbnail-loader".to_string())
            .spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    if let Some(cover) = decode_cover_image(&req.path) {
                        let ctx = req.ctx;
                        if res_tx
                            .send(DecodedThumbnail {
                                sound_id: req.sound_id,
                                path: req.path,
                                cover,
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
            generation: 0,
        }
    }

    /// Drains completed background image decodes and uploads them as GPU textures.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(item) = self.res_rx.try_recv() {
            self.pending.remove(&(item.sound_id.clone(), item.path.clone()));
            self.generation = self.generation.wrapping_add(1);
            let cached = upload_cover_texture(
                ctx,
                &format!("pad_thumb_{}", item.sound_id),
                self.generation,
                item.cover,
            );
            self.textures.insert(item.sound_id, (item.path, cached));
        }
    }

    /// Returns a cached texture if ready (advancing animated GIFs cleanly), or schedules an async decode.
    pub fn texture_for(&mut self, ctx: &egui::Context, sound: &SoundItem) -> Option<&TextureHandle> {
        let Some(path) = sound.resolved_image_path() else {
            self.textures.remove(&sound.id);
            self.pending.retain(|(id, _)| id != &sound.id);
            return None;
        };

        let cache_hit = self
            .textures
            .get(&sound.id)
            .is_some_and(|(cached_path, _)| *cached_path == path);
        if cache_hit {
            let (_, cached) = self.textures.get(&sound.id)?;
            return Some(select_cover_texture(cached, ctx));
        }
        self.textures.remove(&sound.id);

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

    /// Uploads a freshly decoded cover immediately as GPU texture(s) for zero-lag cover updates.
    fn set_decoded(
        &mut self,
        ctx: &egui::Context,
        sound_id: &str,
        path: PathBuf,
        cover: DecodedCover,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let cached = upload_cover_texture(
            ctx,
            &format!("pad_thumb_{sound_id}"),
            self.generation,
            cover,
        );
        self.set_cached(sound_id, path, cached);
    }

    fn set_cached(&mut self, sound_id: &str, path: PathBuf, cached: CachedCover) {
        self.pending.retain(|(id, _)| id != sound_id);
        self.failed.retain(|(id, _)| id != sound_id);
        self.textures.insert(sound_id.to_string(), (path, cached));
    }
}

impl Default for ThumbnailLoader {
    fn default() -> Self {
        Self::new()
    }
}

fn rgba_to_color_image(thumb: RgbaImage) -> ColorImage {
    let size = [thumb.width() as usize, thumb.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, thumb.as_raw())
}

fn normalize_gif_delay(delay: image::Delay) -> Duration {
    let raw = Duration::from(delay);
    if raw < MIN_GIF_DELAY {
        FALLBACK_GIF_DELAY
    } else {
        raw
    }
}

fn decode_gif_frames<R: BufRead + Seek>(reader: R) -> Option<DecodedCover> {
    let decoder = GifDecoder::new(reader).ok()?;
    let mut frames = Vec::new();
    let mut total_duration = Duration::ZERO;

    for frame_res in decoder.into_frames() {
        let Ok(frame) = frame_res else {
            break;
        };
        let delay = normalize_gif_delay(frame.delay());
        let thumb = DynamicImage::ImageRgba8(frame.into_buffer())
            .thumbnail(COVER_THUMB_MAX_DIM, COVER_THUMB_MAX_DIM)
            .to_rgba8();
        total_duration = total_duration.saturating_add(delay);
        frames.push(DecodedGifFrame {
            image: rgba_to_color_image(thumb),
            delay,
        });
    }

    match frames.len() {
        0 => None,
        1 => frames.into_iter().next().map(|f| DecodedCover::Static(f.image)),
        _ => Some(DecodedCover::Animated {
            frames,
            total_duration,
        }),
    }
}

fn decode_cover_from_reader<R: BufRead + Seek>(reader: R) -> Option<DecodedCover> {
    let img_reader = ImageReader::new(reader).with_guessed_format().ok()?;
    if img_reader.format() == Some(ImageFormat::Gif) {
        let mut inner = img_reader.into_inner();
        let _ = inner.seek(SeekFrom::Start(0));
        return decode_gif_frames(inner);
    }
    let decoded = img_reader.decode().ok()?;
    let thumb = decoded
        .thumbnail(COVER_THUMB_MAX_DIM, COVER_THUMB_MAX_DIM)
        .to_rgba8();
    Some(DecodedCover::Static(rgba_to_color_image(thumb)))
}

fn decode_cover_image(path: &Path) -> Option<DecodedCover> {
    let file = File::open(path).ok()?;
    decode_cover_from_reader(BufReader::new(file))
}

fn decode_cover_bytes(bytes: &[u8]) -> Option<DecodedCover> {
    decode_cover_from_reader(Cursor::new(bytes))
}

fn upload_cover_texture(
    ctx: &egui::Context,
    key_prefix: &str,
    generation: u64,
    cover: DecodedCover,
) -> CachedCover {
    match cover {
        DecodedCover::Static(image) => CachedCover::Static(ctx.load_texture(
            format!("{key_prefix}_{generation}_static"),
            image,
            TextureOptions::LINEAR,
        )),
        DecodedCover::Animated {
            frames,
            total_duration,
        } => {
            let uploaded = frames
                .into_iter()
                .enumerate()
                .map(|(idx, frame)| GifFrame {
                    handle: ctx.load_texture(
                        format!("{key_prefix}_{generation}_f{idx}"),
                        frame.image,
                        TextureOptions::LINEAR,
                    ),
                    delay: frame.delay,
                })
                .collect();
            CachedCover::Animated {
                frames: uploaded,
                total_duration,
            }
        }
    }
}

fn active_gif_frame_at<T>(
    frames: &[T],
    delay_of: impl Fn(&T) -> Duration,
    total_duration: Duration,
    time_secs: f64,
) -> (usize, Duration) {
    let total_secs = total_duration.as_secs_f64();
    if frames.len() <= 1 || total_secs <= 0.0 {
        return (0, Duration::ZERO);
    }
    let loop_pos = time_secs.max(0.0).rem_euclid(total_secs);
    let mut accum = 0.0;
    for (idx, frame) in frames.iter().enumerate() {
        let frame_end = accum + delay_of(frame).as_secs_f64();
        if loop_pos < frame_end {
            let remaining = Duration::from_secs_f64((frame_end - loop_pos).max(0.001));
            return (idx, remaining);
        }
        accum = frame_end;
    }
    (0, delay_of(&frames[0]))
}

fn select_cover_texture<'a>(cached: &'a CachedCover, ctx: &egui::Context) -> &'a TextureHandle {
    match cached {
        CachedCover::Static(handle) => handle,
        CachedCover::Animated {
            frames,
            total_duration,
        } => {
            let now = ctx.input(|i| i.time);
            let (idx, remaining) =
                active_gif_frame_at(frames, |f| f.delay, *total_duration, now);
            if !remaining.is_zero() {
                ctx.request_repaint_after(remaining);
            }
            &frames[idx].handle
        }
    }
}

/// Decodes the embedded `assets/logo.png` into a 256x256 RGBA [`egui::IconData`] for the
/// application window titlebar, Windows Taskbar, and non-bundled macOS runs.
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

struct PendingCoverDrop {
    managed_cover: PathBuf,
    cover: DecodedCover,
    preview_cached: Option<CachedCover>,
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
    pub updater: UpdateChecker,
    pub local_ip: String,
    pub last_error: Option<String>,
    last_hovered_image_pad_id: Option<String>,
    last_valid_pointer_pos: Option<egui::Pos2>,
    pending_cover_drop: Option<PendingCoverDrop>,
    status_toast: Option<(String, Instant)>,
    last_playback_state: (bool, Option<String>),
}

impl WaweFxApp {
    pub fn new(
        ctx: &egui::Context,
        config: AppConfig,
        audio: AudioEngine,
        sync: SyncServerHandle,
    ) -> Self {
        audio.set_allow_overlap(config.allow_overlap);
        let cached_devices = audio.available_devices();
        let local_ip = sync.local_ip();
        let last_playback_state = (audio.is_playing(), audio.active_sound_id());
        let mut hotkeys = HotkeyService::new(ctx);
        hotkeys.sync_bindings(&config);
        let updater = UpdateChecker::spawn(ctx.clone());

        Self {
            config,
            audio,
            sync,
            thumbnails: ThumbnailLoader::new(),
            active_modal: None,
            cached_devices,
            driver_setup: DriverSetupState::default(),
            hotkeys,
            updater,
            local_ip,
            last_error: None,
            last_hovered_image_pad_id: None,
            last_valid_pointer_pos: None,
            pending_cover_drop: None,
            status_toast: None,
            last_playback_state,
        }
    }

    fn set_target_pitch(&mut self, next_speed: f32) {
        let clamped = next_speed.clamp(
            crate::config::MIN_PLAYBACK_SPEED,
            crate::config::MAX_PLAYBACK_SPEED,
        );
        self.config.audio.playback_speed = clamped;
        self.audio.set_speed(clamped);
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

        let (trim_start, trim_end) = sanitize_trim_range(draft.trim_start, draft.trim_end);

        if let Some(edit_id) = draft.editing_id {
            if let Some(ref sc) = normalized_shortcut {
                self.config
                    .unbind_shortcut_conflicts(sc, Some(ShortcutOwner::Sound(&edit_id)));
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
                existing.trim_start = trim_start;
                existing.trim_end = trim_end;
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
                    .unbind_shortcut_conflicts(sc, Some(ShortcutOwner::Sound(&new_id)));
            }
            let new_item = SoundItem {
                id: new_id.clone(),
                title,
                audio_path,
                image_path,
                duration_secs: draft.duration_secs,
                volume_trim: draft.volume_trim,
                trim_start,
                trim_end,
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

        let (hovered_files, dropped_files) =
            ctx.input(|i| (i.raw.hovered_files.clone(), i.raw.dropped_files.clone()));
        let active_file_drag = !hovered_files.is_empty() || !dropped_files.is_empty();
        let drop_pointer_pos = resolve_file_drop_pointer_pos(ctx, active_file_drag);
        if let Some(pos) = drop_pointer_pos
            && ctx.screen_rect().contains(pos)
        {
            self.last_valid_pointer_pos = Some(pos);
        }

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
                HotkeyAction::PitchUp => {
                    let next = step_playback_speed(
                        self.config.audio.playback_speed,
                        self.config.audio.pitch_step,
                    );
                    self.set_target_pitch(next);
                    config_dirty = true;
                    sync_dirty = true;
                    ctx.request_repaint();
                }
                HotkeyAction::PitchDown => {
                    let next = step_playback_speed(
                        self.config.audio.playback_speed,
                        -self.config.audio.pitch_step,
                    );
                    self.set_target_pitch(next);
                    config_dirty = true;
                    sync_dirty = true;
                    ctx.request_repaint();
                }
                HotkeyAction::PitchReset => {
                    self.set_target_pitch(1.0);
                    config_dirty = true;
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
                DesktopSyncNotice::SetAllowOverlap(allow) => {
                    self.config.allow_overlap = allow;
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
        let is_any_modal_open = self.active_modal.is_some() || self.pending_cover_drop.is_some();

        // Top Pixel-Accurate Toolbar
        let available_update = self.updater.available_update();
        let tb_action = egui::TopBottomPanel::top("wawefx_toolbar")
            .exact_height(toolbar::TOOLBAR_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(bg_toolbar))
            .show(ctx, |ui| {
                if is_any_modal_open {
                    ui.disable();
                }
                toolbar::render_toolbar(
                    ui,
                    &mut self.config,
                    &self.audio,
                    show_alert_badge,
                    available_update.as_ref(),
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
                if is_any_modal_open {
                    ui.disable();
                }
                toolbar::render_vertical_pitch_panel(ui, &mut self.config, &self.audio)
            })
            .inner;
        if pitch_changed {
            config_dirty = true;
            sync_dirty = true;
        }

        let accent = rgb_to_color32(self.config.theme.accent_color);
        let hovered_audio_drop = !is_any_modal_open
            && hovered_files.iter().any(|f| {
                f.path.as_deref().is_some_and(is_audio_file) || f.mime.starts_with("audio/")
            });
        let hovered_image_drop = !is_any_modal_open
            && hovered_files.iter().any(|f| {
                f.path.as_deref().is_some_and(is_image_file)
                    || f.mime.starts_with("image/")
                    || (f.path.is_none() && f.mime.is_empty())
            });

        // Main Soundboard Canvas / Grid (never shifts layout during external file drag-hover)
        let inner_margin = if self.config.sounds.is_empty() && self.last_error.is_none() {
            0
        } else {
            16
        };
        let panel_resp = egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(bg_main)
                    .inner_margin(egui::Margin::same(inner_margin)),
            )
            .show(ctx, |ui| {
                if is_any_modal_open {
                    ui.disable();
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
                    drop_pointer_pos,
                )
            });
        let central_rect = panel_resp.response.rect;
        let grid_action = panel_resp.inner;

        if hovered_image_drop {
            self.last_hovered_image_pad_id = grid_action.hovered_image_pad_id.clone();
        }

        if grid_action.config_dirty {
            config_dirty = true;
            sync_dirty = true;
        }

        if !is_any_modal_open
            && !dropped_files.is_empty()
            && self.handle_dropped_files(
                ctx,
                &dropped_files,
                drop_pointer_pos,
                &grid_action.pad_rects,
            )
        {
            config_dirty = true;
            sync_dirty = true;
        }
        if !hovered_image_drop {
            self.last_hovered_image_pad_id = None;
        }

        if let Some((_, created_at)) = &self.status_toast
            && created_at.elapsed() >= Duration::from_millis(3500)
        {
            self.status_toast = None;
        }

        if !is_any_modal_open && !hovered_files.is_empty() {
            let hint = if hovered_image_drop && !hovered_audio_drop {
                if let Some(ref pad_id) = grid_action.hovered_image_pad_id
                    && let Some(pad) = self.config.sounds.iter().find(|s| s.id == *pad_id)
                {
                    format!("Release to set cover on '{}'", pad.title)
                } else {
                    "Drop image onto a sound pad (or open Edit Sound first)".to_string()
                }
            } else if hovered_audio_drop && !hovered_image_drop {
                "Drop audio file anywhere to add a new Sound Pad".to_string()
            } else {
                "Drop image onto a sound pad (or open Edit Sound first)".to_string()
            };
            paint_top_drop_banner(
                ctx,
                central_rect,
                &hint,
                accent,
                rgb_to_color32(self.config.theme.bg_toolbar),
                rgb_to_color32(self.config.theme.pad_text),
            );
        } else if let Some((ref toast, _)) = self.status_toast {
            paint_floating_hint_pill(
                ctx,
                central_rect,
                toast,
                accent,
                rgb_to_color32(self.config.theme.bg_toolbar),
                rgb_to_color32(self.config.theme.pad_text),
            );
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

        // Render dimmed backdrop & active modal window (or pending dropped-cover target picker)
        if self.active_modal.is_some() || self.pending_cover_drop.is_some() {
            render_modal_backdrop(ctx);
        }
        if let Some(mut modal) = self.active_modal.take() {
            if let ActiveModal::SoundEditor(ref mut draft) = modal
                && !dropped_files.is_empty()
            {
                self.handle_dropped_files_in_modal(draft, &dropped_files);
            }

            let bound_port = self.sync.bound_port();
            let connected_clients = self.sync.connected_clients();
            let mut modal_cfg = self.config.clone();
            modal_cfg.audio.sync_port = bound_port;

            let update_state = self.updater.check_state();
            let outcome = modals::render_modals(
                ctx,
                &mut modal,
                &mut modal_cfg,
                &self.audio,
                &self.cached_devices,
                &status,
                self.hotkeys.status(),
                &self.driver_setup,
                &update_state,
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
            } else {
                if self.audio.active_sound_id().as_deref() == Some(PREVIEW_SOUND_ID) {
                    self.audio.stop();
                }
                ctx.request_repaint();
            }
        } else if self.pending_cover_drop.is_some()
            && self.render_pending_cover_drop_picker(ctx, &grid_action.pad_rects)
        {
            config_dirty = true;
            sync_dirty = true;
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
        // keep external drag-and-drop hover highlighting responsive at ~60 Hz,
        // and refresh live callback telemetry or background setup progress.
        if config_dirty || sync_dirty {
            ctx.request_repaint();
        }
        if !hovered_files.is_empty() {
            ctx.request_repaint();
            ctx.request_repaint_after(Duration::from_millis(16));
        } else if self.audio.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.driver_setup.is_running() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if self.status_toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else if matches!(self.active_modal, Some(ActiveModal::Alerts)) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl WaweFxApp {
    fn apply_cover_to_pad(
        &mut self,
        ctx: &egui::Context,
        pad_id: &str,
        managed_cover: PathBuf,
        cover: DecodedCover,
        preuploaded: Option<CachedCover>,
    ) -> bool {
        let resolved_managed = AppConfig::resolve_asset_path(&managed_cover);
        let mut pad_title = String::new();
        let mut applied = false;
        let old_img = if let Some(sound) = self.config.sounds.iter_mut().find(|s| s.id == pad_id) {
            applied = true;
            pad_title = sound.title.clone();
            let prev = sound.image_path.replace(managed_cover.clone());
            if prev.as_ref() == Some(&managed_cover) {
                None
            } else {
                prev
            }
        } else {
            None
        };
        if !applied {
            self.config.cleanup_managed_path(&managed_cover);
            return false;
        }
        if let Some(prev_path) = old_img {
            self.config.cleanup_managed_path(&prev_path);
        }
        if let Some(cached) = preuploaded {
            self.thumbnails.set_cached(pad_id, resolved_managed, cached);
        } else {
            self.thumbnails
                .set_decoded(ctx, pad_id, resolved_managed, cover);
        }
        self.sync.invalidate_thumbnail(pad_id);
        self.last_error = None;
        self.status_toast = Some((format!("Updated cover for '{}'", pad_title), Instant::now()));
        true
    }

    fn handle_dropped_files_in_modal(
        &mut self,
        draft: &mut SoundModalDraft,
        dropped_files: &[egui::DroppedFile],
    ) {
        for dropped in dropped_files {
            if is_dropped_image(dropped) {
                if let Some(ref path) = dropped.path
                    && dropped.bytes.is_none()
                    && decode_cover_image(path).is_some()
                {
                    draft.image_path = Some(path.clone());
                    draft.error_message = None;
                    self.status_toast =
                        Some(("Assigned dropped cover image".to_string(), Instant::now()));
                    return;
                }
                match import_and_decode_dropped_cover(dropped) {
                    Ok((managed_cover, _)) => {
                        draft.image_path = Some(managed_cover);
                        draft.error_message = None;
                        self.status_toast =
                            Some(("Assigned dropped cover image".to_string(), Instant::now()));
                        return;
                    }
                    Err(err) => {
                        draft.error_message = Some(err);
                    }
                }
            } else if let Some(ref path) = dropped.path
                && is_audio_file(path)
            {
                if draft.title.trim().is_empty()
                    && let Some(stem) = path.file_stem()
                {
                    draft.title = stem.to_string_lossy().to_string();
                }
                match self.audio.probe_audio_with_peaks(path) {
                    Ok((secs, peaks)) => {
                        draft.duration_secs = secs;
                        draft.trim_start = 0.0;
                        draft.trim_end = 1.0;
                        draft.waveform_peaks = Some(peaks);
                        draft.waveform_loaded_for = Some(path.clone());
                        draft.audio_path = Some(path.clone());
                        draft.error_message = None;
                    }
                    Err(err) => {
                        draft.error_message = Some(format!("Could not decode audio: {err}"));
                    }
                }
                return;
            }
        }
    }

    fn render_pending_cover_drop_picker(
        &mut self,
        ctx: &egui::Context,
        pad_rects: &[(String, egui::Rect)],
    ) -> bool {
        let Some(mut pending) = self.pending_cover_drop.take() else {
            return false;
        };

        let preview_cached = pending.preview_cached.get_or_insert_with(|| {
            upload_cover_texture(ctx, "wawefx_pending_cover_drop_preview", 0, pending.cover.clone())
        });
        let tex_id = select_cover_texture(preview_cached, ctx).id();

        let suggested_pad_id =
            find_drop_target_pad(self.last_valid_pointer_pos, pad_rects, None)
                .or_else(|| self.config.selected_sound_id.clone());

        let mut chosen_pad_id: Option<String> = None;
        let mut cancel = false;
        let mut open = true;
        let theme = &self.config.theme;
        let bg_fill = rgb_to_color32(theme.bg_toolbar);
        let stroke_col = rgb_to_color32(theme.toolbar_divider);
        let accent = rgb_to_color32(theme.accent_color);

        let win_resp = egui::Window::new("Select target pad for dropped cover image")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(bg_fill)
                    .stroke(egui::Stroke::new(1.0_f32, stroke_col))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(16)),
            )
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new((tex_id, egui::vec2(56.0, 56.0))).corner_radius(6.0));
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Choose which Sound Pad gets this cover image:")
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Tip: You can also drop directly into Edit Sound Properties.",
                            )
                            .weak()
                            .size(11.0),
                        );
                    });
                });
                ui.add_space(10.0);

                if let Some(ref sug_id) = suggested_pad_id
                    && let Some(sug_pad) = self.config.sounds.iter().find(|s| s.id == *sug_id)
                {
                    let btn_label = format!("Set on Selected/Last Pad: {}", sug_pad.title);
                    let fg = {
                        let luma = 0.299 * accent.r() as f32
                            + 0.587 * accent.g() as f32
                            + 0.114 * accent.b() as f32;
                        if luma > 175.0 {
                            egui::Color32::from_rgb(0x2B, 0x25, 0x20)
                        } else {
                            egui::Color32::WHITE
                        }
                    };
                    if ui
                        .add_sized(
                            egui::vec2(328.0, 28.0),
                            egui::Button::new(egui::RichText::new(btn_label).strong().color(fg))
                                .fill(accent),
                        )
                        .clicked()
                    {
                        chosen_pad_id = Some(sug_pad.id.clone());
                    }
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                }

                egui::ScrollArea::vertical()
                    .max_height(210.0)
                    .show(ui, |ui| {
                        for sound in &self.config.sounds {
                            if ui
                                .add_sized(egui::vec2(328.0, 26.0), egui::Button::new(&sound.title))
                                .clicked()
                            {
                                chosen_pad_id = Some(sound.id.clone());
                            }
                        }
                    });

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if let Some(resp) = win_resp {
            ctx.move_to_top(resp.response.layer_id);
        }

        if let Some(pad_id) = chosen_pad_id {
            return self.apply_cover_to_pad(
                ctx,
                &pad_id,
                pending.managed_cover,
                pending.cover,
                pending.preview_cached.take(),
            );
        }
        if !open || cancel {
            self.config.cleanup_managed_path(&pending.managed_cover);
            return false;
        }
        self.pending_cover_drop = Some(pending);
        false
    }

    /// Processes external audio/image files dropped onto the application window.
    fn handle_dropped_files(
        &mut self,
        ctx: &egui::Context,
        dropped_files: &[egui::DroppedFile],
        drop_pos: Option<egui::Pos2>,
        pad_rects: &[(String, egui::Rect)],
    ) -> bool {
        let mut changed = false;

        for dropped in dropped_files {
            if let Some(ref path) = dropped.path
                && is_audio_file(path)
            {
                match AudioEngine::probe_audio_duration(path) {
                    Ok(duration_secs) => {
                        let title = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or("New Sound")
                            .to_string();
                        match AppConfig::import_sound_file(path) {
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
            } else if is_dropped_image(dropped) {
                let (managed_cover, cover) = match import_and_decode_dropped_cover(dropped) {
                    Ok(res) => res,
                    Err(err) => {
                        self.last_error = Some(err);
                        continue;
                    }
                };

                #[cfg(target_os = "macos")]
                let resolved_drop_pos = {
                    let os_pos = macos_live_cursor_in_window(ctx);
                    let latest_pos = ctx.input(|i| i.pointer.latest_pos());
                    let inner_rect = ctx.input(|i| i.viewport().inner_rect);
                    eprintln!(
                        "[macOS D&D] os_pos={os_pos:?} latest_pos={latest_pos:?} inner_rect={inner_rect:?} pad_count={}",
                        pad_rects.len()
                    );
                    os_pos.or(drop_pos).or(latest_pos)
                };
                #[cfg(not(target_os = "macos"))]
                let resolved_drop_pos = drop_pos.or_else(|| resolve_file_drop_pointer_pos(ctx, true));
                let target_pad_id = find_drop_target_pad(
                    resolved_drop_pos,
                    pad_rects,
                    self.last_hovered_image_pad_id.as_deref(),
                )
                .or_else(|| {
                    (self.config.sounds.len() == 1).then(|| self.config.sounds[0].id.clone())
                });

                if let Some(pad_id) = target_pad_id {
                    if self.apply_cover_to_pad(ctx, &pad_id, managed_cover, cover, None) {
                        changed = true;
                    }
                } else if self.config.sounds.is_empty() {
                    self.config.cleanup_managed_path(&managed_cover);
                    self.status_toast = Some((
                        "Add a sound pad first, then drop a cover image onto it.".to_string(),
                        Instant::now(),
                    ));
                } else {
                    if let Some(prev_pending) = self.pending_cover_drop.take() {
                        self.config.cleanup_managed_path(&prev_pending.managed_cover);
                    }
                    self.last_error = None;
                    self.pending_cover_drop = Some(PendingCoverDrop {
                        managed_cover,
                        cover,
                        preview_cached: None,
                    });
                }
            } else if let Some(ref path) = dropped.path {
                self.last_error = Some(format!(
                    "Unsupported file type '{}'. Supported: mp3, wav, ogg, flac, png, jpg, jpeg, webp, bmp, gif.",
                    path.display()
                ));
            } else {
                self.last_error =
                    Some("Dropped item is not a supported audio or image format.".to_string());
            }
        }

        changed
    }
}

fn is_dropped_image(dropped: &egui::DroppedFile) -> bool {
    dropped.path.as_deref().is_some_and(is_image_file)
        || dropped.mime.starts_with("image/")
        || dropped
            .bytes
            .as_deref()
            .is_some_and(|b| image::guess_format(b).is_ok())
}

fn import_and_decode_dropped_cover(
    dropped: &egui::DroppedFile,
) -> Result<(PathBuf, DecodedCover), String> {
    if let Some(bytes) = dropped.bytes.as_deref() {
        let cover = decode_cover_bytes(bytes)
            .ok_or_else(|| "Could not decode dropped image bytes.".to_string())?;
        let hint = dropped
            .path
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .or_else(|| (!dropped.name.trim().is_empty()).then_some(dropped.name.as_str()));
        let managed_cover = AppConfig::import_cover_bytes(bytes, hint).map_err(|err| {
            format!("Failed to save dropped cover into WaweFX data folder: {err:#}")
        })?;
        return Ok((managed_cover, cover));
    }

    if let Some(ref path) = dropped.path {
        let cover = decode_cover_image(path)
            .ok_or_else(|| format!("Could not decode dropped image '{}'.", path.display()))?;
        let managed_cover = AppConfig::import_cover_file(path).map_err(|err| {
            format!(
                "Failed to copy dropped cover '{}' into WaweFX data folder: {err:#}",
                path.display()
            )
        })?;
        return Ok((managed_cover, cover));
    }

    Err("Dropped image does not contain a file path or raw bytes.".to_string())
}

fn render_modal_backdrop(ctx: &egui::Context) {
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("wawefx_modal_backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .movable(false)
        .interactable(true)
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            ui.painter().rect_filled(
                screen,
                egui::CornerRadius::ZERO,
                egui::Color32::from_black_alpha(110),
            );
            ui.allocate_rect(screen, egui::Sense::click_and_drag());
        });
}

fn resolve_file_drop_pointer_pos(
    ctx: &egui::Context,
    active_file_drag: bool,
) -> Option<egui::Pos2> {
    if active_file_drag {
        #[cfg(target_os = "macos")]
        if let Some(mac_pos) = macos_live_cursor_in_window(ctx) {
            return Some(mac_pos);
        }

        #[cfg(target_os = "windows")]
        if let Some(win_pos) = windows_live_cursor_in_window(ctx) {
            return Some(win_pos);
        }
    }

    ctx.input(|i| {
        i.pointer
            .latest_pos()
            .or_else(|| i.pointer.interact_pos())
            .or_else(|| i.pointer.hover_pos())
    })
}

#[cfg(target_os = "macos")]
fn macos_live_cursor_in_window(ctx: &egui::Context) -> Option<egui::Pos2> {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *const c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    let inner_rect = ctx.input(|i| i.viewport().inner_rect)?;
    // SAFETY: `CGEventCreate(null)` allocates an immutable snapshot event containing the live
    // hardware cursor position; when non-null we read its location immediately and release it.
    let pt = unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let loc = CGEventGetLocation(event);
        CFRelease(event);
        loc
    };
    let ppp = ctx.pixels_per_point().max(0.1);
    let native_scale = ctx
        .input(|i| i.viewport().native_pixels_per_point)
        .unwrap_or(ppp)
        .max(0.1);
    let zoom_adjust = (native_scale / ppp) as f64;
    macos_screen_to_window_local(
        (pt.x * zoom_adjust, pt.y * zoom_adjust),
        inner_rect,
        ctx.screen_rect(),
        ppp,
    )
}

#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn macos_screen_to_window_local(
    cursor_screen_pts: (f64, f64),
    inner_rect: egui::Rect,
    screen_rect: egui::Rect,
    ppp: f32,
) -> Option<egui::Pos2> {
    let bounds = screen_rect.expand(6.0);
    let local_pts = egui::pos2(
        cursor_screen_pts.0 as f32 - inner_rect.min.x,
        cursor_screen_pts.1 as f32 - inner_rect.min.y,
    );
    if bounds.contains(local_pts) {
        return Some(local_pts);
    }

    let safe_ppp = ppp.max(0.1);
    if (safe_ppp - 1.0).abs() > f32::EPSILON {
        let local_phys = egui::pos2(
            cursor_screen_pts.0 as f32 / safe_ppp - inner_rect.min.x,
            cursor_screen_pts.1 as f32 / safe_ppp - inner_rect.min.y,
        );
        if bounds.contains(local_phys) {
            return Some(local_phys);
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn windows_live_cursor_in_window(ctx: &egui::Context) -> Option<egui::Pos2> {
    #[repr(C)]
    struct WinPoint {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetCursorPos(lp_point: *mut WinPoint) -> i32;
    }

    let inner_rect = ctx.input(|i| i.viewport().inner_rect)?;
    let mut pt = WinPoint { x: 0, y: 0 };
    // SAFETY: `pt` points to valid stack-allocated WinPoint memory for GetCursorPos.
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return None;
    }
    let ppp = ctx.pixels_per_point().max(0.1);
    let local = egui::pos2(
        pt.x as f32 / ppp - inner_rect.min.x,
        pt.y as f32 / ppp - inner_rect.min.y,
    );
    ctx.screen_rect().expand(4.0).contains(local).then_some(local)
}

fn find_drop_target_pad(
    drop_pos: Option<egui::Pos2>,
    pad_rects: &[(String, egui::Rect)],
    fallback_hovered_pad_id: Option<&str>,
) -> Option<String> {
    if let Some(pos) = drop_pos {
        if let Some((id, _)) = pad_rects.iter().find(|(_, rect)| rect.contains(pos)) {
            return Some(id.clone());
        }
        return pad_rects
            .iter()
            .filter(|(_, rect)| rect.expand(6.0).contains(pos))
            .min_by(|(_, a), (_, b)| {
                a.center()
                    .distance_sq(pos)
                    .total_cmp(&b.center().distance_sq(pos))
            })
            .map(|(id, _)| id.clone());
    }

    fallback_hovered_pad_id.and_then(|hovered_id| {
        pad_rects
            .iter()
            .find(|(id, _)| id == hovered_id)
            .map(|(id, _)| id.clone())
    })
}

fn paint_top_drop_banner(
    ctx: &egui::Context,
    canvas_rect: egui::Rect,
    text: &str,
    accent: egui::Color32,
    surface: egui::Color32,
    text_color: egui::Color32,
) {
    let center_y = (canvas_rect.min.y + 22.0).min(canvas_rect.max.y - 18.0);
    paint_pill_at_y(ctx, canvas_rect, center_y, "wawefx_drop_top_banner", text, accent, surface, text_color);
}

fn paint_floating_hint_pill(
    ctx: &egui::Context,
    canvas_rect: egui::Rect,
    text: &str,
    accent: egui::Color32,
    surface: egui::Color32,
    text_color: egui::Color32,
) {
    let center_y = (canvas_rect.max.y - 24.0).max(canvas_rect.min.y + 20.0);
    paint_pill_at_y(ctx, canvas_rect, center_y, "wawefx_drop_hint_pill", text, accent, surface, text_color);
}

#[allow(clippy::too_many_arguments)]
fn paint_pill_at_y(
    ctx: &egui::Context,
    canvas_rect: egui::Rect,
    center_y: f32,
    id_salt: &str,
    text: &str,
    accent: egui::Color32,
    surface: egui::Color32,
    text_color: egui::Color32,
) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new(id_salt),
    ));
    let font = egui::FontId::proportional(12.5);
    let galley = painter.layout_no_wrap(text.to_string(), font, text_color);
    let max_w = (canvas_rect.width() - 32.0).max(160.0);
    let pill_w = (galley.size().x + 28.0).clamp(140.0, max_w);
    let pill_h = 30.0;
    let center = egui::pos2(canvas_rect.center().x, center_y);
    let pill_rect = egui::Rect::from_center_size(center, egui::vec2(pill_w, pill_h));
    let bg = egui::Color32::from_rgba_unmultiplied(surface.r(), surface.g(), surface.b(), 238);
    painter.rect_filled(pill_rect, egui::CornerRadius::same(15), bg);
    painter.rect_stroke(
        pill_rect,
        egui::CornerRadius::same(15),
        egui::Stroke::new(1.5_f32, accent),
        egui::StrokeKind::Inside,
    );
    let text_pos = egui::pos2(
        pill_rect.center().x - galley.size().x * 0.5,
        pill_rect.center().y - galley.size().y * 0.5,
    );
    painter.galley(text_pos, galley, text_color);
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
    has_extension_in(path, &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::path::Path;
    use std::time::Duration;

    use image::codecs::gif::GifEncoder;
    use image::codecs::ico::{IcoDecoder, IcoEncoder, IcoFrame};
    use image::codecs::png::PngEncoder;
    use image::imageops::FilterType;
    use image::{Delay, DynamicImage, ExtendedColorType, Frame, ImageDecoder, ImageEncoder, Rgba, RgbaImage};

    use super::{
        DecodedCover, LOGO_PNG_BYTES, active_gif_frame_at, build_window_icon, decode_cover_bytes,
        decode_cover_image, find_drop_target_pad, is_image_file, macos_screen_to_window_local,
    };

    #[test]
    fn image_file_detection_is_case_insensitive_including_bmp_and_gif() {
        for ext in ["png", "PNG", "jpg", "JPEG", "webp", "WebP", "bmp", "BMP", "gif", "GIF"] {
            assert!(is_image_file(Path::new(&format!("cover.{ext}"))));
        }
        assert!(!is_image_file(Path::new("sound.wav")));
        assert!(!is_image_file(Path::new("no_ext")));
    }

    #[test]
    fn decodes_bmp_thumbnail_image() {
        let temp_bmp =
            std::env::temp_dir().join(format!("wawefx_test_cover_{}.bmp", uuid::Uuid::new_v4()));
        let img = RgbaImage::from_pixel(512, 512, Rgba([64, 180, 240, 255]));
        img.save(&temp_bmp).expect("save temp bmp");
        let decoded = decode_cover_image(&temp_bmp);
        let _ = fs::remove_file(&temp_bmp);
        let DecodedCover::Static(color_img) = decoded.expect("bmp image should decode") else {
            panic!("bmp should decode as static cover");
        };
        assert_eq!(color_img.size, [256, 256]);
        assert_eq!(
            color_img.pixels[0],
            eframe::egui::Color32::from_rgba_unmultiplied(64, 180, 240, 255)
        );
    }

    #[test]
    fn decodes_thumbnail_from_raw_image_bytes() {
        let img = RgbaImage::from_pixel(512, 512, Rgba([220, 90, 40, 255]));
        let mut png_bytes = Vec::new();
        PngEncoder::new(&mut png_bytes)
            .write_image(img.as_raw(), 512, 512, ExtendedColorType::Rgba8)
            .expect("encode png bytes");
        let DecodedCover::Static(decoded) =
            decode_cover_bytes(&png_bytes).expect("bytes should decode")
        else {
            panic!("png should decode as static cover");
        };
        assert_eq!(decoded.size, [256, 256]);
        assert_eq!(
            decoded.pixels[0],
            eframe::egui::Color32::from_rgba_unmultiplied(220, 90, 40, 255)
        );
        assert!(decode_cover_bytes(b"invalid image data").is_none());
    }

    #[test]
    fn decodes_animated_gif_frames_downscales_clamps_delays_and_computes_loop_index() {
        let mut gif_bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif_bytes);
            let f1 = Frame::from_parts(
                RgbaImage::from_pixel(400, 300, Rgba([255, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(0, 1),
            );
            let f2 = Frame::from_parts(
                RgbaImage::from_pixel(400, 300, Rgba([0, 255, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(10, 1),
            );
            let f3 = Frame::from_parts(
                RgbaImage::from_pixel(400, 300, Rgba([0, 0, 255, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(90, 1),
            );
            encoder.encode_frames([f1, f2, f3]).expect("encode 3-frame gif");
        }

        let DecodedCover::Animated {
            frames,
            total_duration,
        } = decode_cover_bytes(&gif_bytes).expect("gif should decode")
        else {
            panic!("multi-frame gif must decode as animated");
        };

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].image.size, [256, 192]);
        assert_eq!(frames[1].image.size, [256, 192]);
        assert_eq!(frames[2].image.size, [256, 192]);
        assert_eq!(frames[0].delay, Duration::from_millis(50));
        assert_eq!(frames[1].delay, Duration::from_millis(50));
        assert_eq!(frames[2].delay, Duration::from_millis(90));
        assert_eq!(total_duration, Duration::from_millis(190));

        assert_eq!(
            active_gif_frame_at(&frames, |f| f.delay, total_duration, 0.010),
            (0, Duration::from_millis(40))
        );
        assert_eq!(
            active_gif_frame_at(&frames, |f| f.delay, total_duration, 0.060),
            (1, Duration::from_millis(40))
        );
        assert_eq!(
            active_gif_frame_at(&frames, |f| f.delay, total_duration, 0.120),
            (2, Duration::from_millis(70))
        );
        // Loop wraps cleanly past total_duration (0.200 % 0.190 == 0.010)
        let (wrapped_idx, wrapped_rem) =
            active_gif_frame_at(&frames, |f| f.delay, total_duration, 0.200);
        assert_eq!(wrapped_idx, 0);
        assert!((wrapped_rem.as_secs_f64() - 0.040).abs() < 0.002);
    }

    #[test]
    fn find_drop_target_pad_handles_exact_tolerance_outside_and_fallback() {
        let rects = vec![
            (
                "pad-1".to_string(),
                eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(10.0, 10.0),
                    eframe::egui::pos2(110.0, 140.0),
                ),
            ),
            (
                "pad-2".to_string(),
                eframe::egui::Rect::from_min_max(
                    eframe::egui::pos2(124.0, 10.0),
                    eframe::egui::pos2(224.0, 140.0),
                ),
            ),
        ];

        assert_eq!(
            find_drop_target_pad(Some(eframe::egui::pos2(50.0, 50.0)), &rects, None).as_deref(),
            Some("pad-1")
        );
        // 5 px outside pad-2's left edge matches via 6 px tolerance
        assert_eq!(
            find_drop_target_pad(Some(eframe::egui::pos2(119.0, 50.0)), &rects, None).as_deref(),
            Some("pad-2")
        );
        // Far outside any card returns None rather than falling back wrongly
        assert_eq!(
            find_drop_target_pad(
                Some(eframe::egui::pos2(400.0, 400.0)),
                &rects,
                Some("pad-1")
            ),
            None
        );
        // When OS gives no pointer coordinates on drop, uses the last hovered visible pad
        assert_eq!(
            find_drop_target_pad(None, &rects, Some("pad-2")).as_deref(),
            Some("pad-2")
        );
        assert_eq!(
            find_drop_target_pad(None, &rects, Some("scrolled-off-pad")),
            None
        );
    }

    #[test]
    fn macos_screen_to_window_local_handles_logical_retina_and_out_of_bounds() {
        let inner_rect = eframe::egui::Rect::from_min_size(
            eframe::egui::pos2(200.0, 120.0),
            eframe::egui::vec2(800.0, 600.0),
        );
        let screen_rect = eframe::egui::Rect::from_min_size(
            eframe::egui::pos2(0.0, 0.0),
            eframe::egui::vec2(800.0, 600.0),
        );
        let pad_rect = eframe::egui::Rect::from_min_max(
            eframe::egui::pos2(124.0, 100.0),
            eframe::egui::pos2(224.0, 230.0),
        );
        let rects = vec![("pad-2".to_string(), pad_rect)];

        // Standard CoreGraphics Quartz global points -> exact window-local coordinate hitting pad-2
        let local = macos_screen_to_window_local((360.0, 270.0), inner_rect, screen_rect, 2.0)
            .expect("logical cursor should map inside window");
        assert_eq!(local, eframe::egui::pos2(160.0, 150.0));
        assert_eq!(
            find_drop_target_pad(Some(local), &rects, None).as_deref(),
            Some("pad-2")
        );

        // Edge within 6 px tolerance inside window bounds
        assert_eq!(
            macos_screen_to_window_local((196.0, 120.0), inner_rect, screen_rect, 1.0),
            Some(eframe::egui::pos2(-4.0, 0.0))
        );

        // Physical pixel fallback when a Retina physical cursor exceeds logical bounds
        let phys_local =
            macos_screen_to_window_local((1120.0, 1080.0), inner_rect, screen_rect, 2.0)
                .expect("retina physical cursor should fall back to window-local");
        assert_eq!(phys_local, eframe::egui::pos2(360.0, 420.0));

        // Cursor far outside the window rejects instead of returning a bogus coordinate
        assert_eq!(
            macos_screen_to_window_local((2500.0, 1600.0), inner_rect, screen_rect, 2.0),
            None
        );
    }

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
