use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use eframe::egui;
use image::{ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::audio::AudioEngine;
use crate::config::{AppConfig, MAX_PLAYBACK_SPEED, MIN_PLAYBACK_SPEED, SoundItem, ThemeConfig};

/// Hex-encoded UI theme colors synchronized live from desktop Settings to mobile web clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncThemeDto {
    pub bg_main: String,
    pub bg_toolbar: String,
    pub toolbar_divider: String,
    pub icon_primary: String,
    pub icon_secondary: String,
    pub gauge_and_track: String,
    pub accent_color: String,
    pub pad_card_bg: String,
    pub pad_card_hover: String,
    pub pad_text: String,
}

impl SyncThemeDto {
    pub fn from_theme(theme: &ThemeConfig) -> Self {
        Self {
            bg_main: rgb_to_hex(theme.bg_main),
            bg_toolbar: rgb_to_hex(theme.bg_toolbar),
            toolbar_divider: rgb_to_hex(theme.toolbar_divider),
            icon_primary: rgb_to_hex(theme.icon_primary),
            icon_secondary: rgb_to_hex(theme.icon_secondary),
            gauge_and_track: rgb_to_hex(theme.gauge_and_track),
            accent_color: rgb_to_hex(theme.accent_color),
            pad_card_bg: rgb_to_hex(theme.pad_card_bg),
            pad_card_hover: rgb_to_hex(theme.pad_card_hover),
            pad_text: rgb_to_hex(theme.pad_text),
        }
    }
}

fn rgb_to_hex(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

/// Sound entry serialized for mobile / Android companion clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncSoundDto {
    pub id: String,
    pub title: String,
    pub duration: String,
    pub duration_secs: f32,
    pub has_thumbnail: bool,
    pub thumbnail_url: Option<String>,
}

/// Complete initial snapshot sent upon WebSocket connection or `/api/state`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncStatePayload {
    pub sounds: Vec<SyncSoundDto>,
    pub active_sound_id: Option<String>,
    pub selected_sound_id: Option<String>,
    pub is_playing: bool,
    pub progress: f32,
    pub duration_secs: f32,
    pub speed: f32,
    pub mic_enabled: bool,
    pub headphones_enabled: bool,
    pub mic_volume: f32,
    pub headphones_volume: f32,
    pub theme: SyncThemeDto,
    pub revision: u64,
}

/// Structural update broadcast ONLY when sounds (add/edit/delete/reorder) or theme colors change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SoundsListPayload {
    pub sounds: Vec<SyncSoundDto>,
    pub theme: SyncThemeDto,
    pub revision: u64,
}

/// Lightweight telemetry broadcast during playback, seeking, pitch changes, or mute toggles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackTickPayload {
    pub active_sound_id: Option<String>,
    pub selected_sound_id: Option<String>,
    pub is_playing: bool,
    pub progress: f32,
    pub duration_secs: f32,
    pub speed: f32,
    pub mic_enabled: bool,
    pub headphones_enabled: bool,
}

/// WebSocket event envelope pushed from PC -> Mobile companion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum SyncEvent {
    StateUpdated(SyncStatePayload),
    SoundsListUpdated(SoundsListPayload),
    PlaybackTick(PlaybackTickPayload),
}

/// WebSocket commands accepted from Mobile companion -> PC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd")]
pub enum RemoteCommand {
    PlaySound {
        id: String,
    },
    Stop,
    Rewind,
    FastForward,
    Seek {
        progress: f32,
    },
    SetPitch {
        #[serde(alias = "value")]
        speed: f32,
    },
    ToggleMic,
    ToggleHeadphones,
}

/// Configuration synchronization notices applied on the desktop UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopSyncNotice {
    SelectSound(String),
    SetPitch(f32),
    SetMicEnabled(bool),
    SetHeadphonesEnabled(bool),
}

struct CachedThumbBytes {
    source_path: PathBuf,
    content_type: &'static str,
    bytes: Bytes,
}

struct SyncShared {
    audio: AudioEngine,
    sounds: RwLock<Vec<SoundItem>>,
    selected_sound_id: RwLock<Option<String>>,
    theme: RwLock<ThemeConfig>,
    thumb_cache: RwLock<HashMap<String, CachedThumbBytes>>,
    latest_state: RwLock<SyncStatePayload>,
    struct_revision: AtomicU64,
    bound_port: AtomicU16,
    connected_clients: AtomicUsize,
    egui_ctx: Mutex<Option<egui::Context>>,
    desktop_notices: Mutex<Vec<DesktopSyncNotice>>,
    broadcast_tx: broadcast::Sender<String>,
}

impl SyncShared {
    fn new(initial_config: &AppConfig, audio: AudioEngine) -> Self {
        let (broadcast_tx, _) = broadcast::channel::<String>(64);
        let initial_payload = build_state_payload(
            &initial_config.sounds,
            initial_config.selected_sound_id.clone(),
            &initial_config.theme,
            &audio,
            1,
        );

        Self {
            audio,
            sounds: RwLock::new(initial_config.sounds.clone()),
            selected_sound_id: RwLock::new(initial_config.selected_sound_id.clone()),
            theme: RwLock::new(initial_config.theme.clone()),
            thumb_cache: RwLock::new(HashMap::new()),
            latest_state: RwLock::new(initial_payload),
            struct_revision: AtomicU64::new(1),
            bound_port: AtomicU16::new(initial_config.audio.sync_port),
            connected_clients: AtomicUsize::new(0),
            egui_ctx: Mutex::new(None),
            desktop_notices: Mutex::new(Vec::new()),
            broadcast_tx,
        }
    }

    fn request_desktop_repaint(&self) {
        if let Ok(guard) = self.egui_ctx.lock()
            && let Some(ctx) = guard.as_ref()
        {
            ctx.request_repaint();
        }
    }

    fn push_desktop_notice(&self, notice: DesktopSyncNotice) {
        if let Ok(mut guard) = self.desktop_notices.lock() {
            guard.push(notice);
        }
        self.request_desktop_repaint();
    }

    /// Broadcasts a structural `SoundsListUpdated` event (incrementing `struct_revision`)
    /// followed by a `PlaybackTick`, used only when the sound list or theme changes.
    fn broadcast_structural_update(&self) {
        let rev = self.struct_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let sounds = self.sounds.read().map(|g| g.clone()).unwrap_or_default();
        let selected = self
            .selected_sound_id
            .read()
            .ok()
            .and_then(|g| g.clone());
        let theme = self
            .theme
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let full_payload = build_state_payload(&sounds, selected, &theme, &self.audio, rev);
        let list_payload = SoundsListPayload {
            sounds: full_payload.sounds.clone(),
            theme: full_payload.theme.clone(),
            revision: rev,
        };
        let tick_payload = tick_from_state(&full_payload);

        if let Ok(mut guard) = self.latest_state.write() {
            *guard = full_payload;
        }

        if let Ok(json) = serde_json::to_string(&SyncEvent::SoundsListUpdated(list_payload)) {
            let _ = self.broadcast_tx.send(json);
        }
        if let Ok(json) = serde_json::to_string(&SyncEvent::PlaybackTick(tick_payload)) {
            let _ = self.broadcast_tx.send(json);
        }
    }

    /// Broadcasts a lightweight `PlaybackTick` event without bumping `struct_revision`
    /// or re-sending the sound list / thumbnail URLs.
    fn broadcast_playback_tick(&self) {
        let sounds = self.sounds.read().map(|g| g.clone()).unwrap_or_default();
        let selected = self
            .selected_sound_id
            .read()
            .ok()
            .and_then(|g| g.clone());
        let tick = build_playback_tick(&sounds, selected, &self.audio);

        if let Ok(mut guard) = self.latest_state.write() {
            guard.active_sound_id = tick.active_sound_id.clone();
            guard.selected_sound_id = tick.selected_sound_id.clone();
            guard.is_playing = tick.is_playing;
            guard.progress = tick.progress;
            guard.duration_secs = tick.duration_secs;
            guard.speed = tick.speed;
            guard.mic_enabled = tick.mic_enabled;
            guard.headphones_enabled = tick.headphones_enabled;
            guard.mic_volume = self.audio.mic_volume();
            guard.headphones_volume = self.audio.headphones_volume();
        }

        if let Ok(json) = serde_json::to_string(&SyncEvent::PlaybackTick(tick)) {
            let _ = self.broadcast_tx.send(json);
        }
    }

    fn latest_event_json(&self) -> anyhow::Result<String> {
        let payload = self
            .latest_state
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| {
                build_state_payload(&[], None, &ThemeConfig::default(), &self.audio, 0)
            });
        Ok(serde_json::to_string(&SyncEvent::StateUpdated(payload))?)
    }
}

fn tick_from_state(state: &SyncStatePayload) -> PlaybackTickPayload {
    PlaybackTickPayload {
        active_sound_id: state.active_sound_id.clone(),
        selected_sound_id: state.selected_sound_id.clone(),
        is_playing: state.is_playing,
        progress: state.progress,
        duration_secs: state.duration_secs,
        speed: state.speed,
        mic_enabled: state.mic_enabled,
        headphones_enabled: state.headphones_enabled,
    }
}

fn build_playback_tick(
    sounds: &[SoundItem],
    selected_sound_id: Option<String>,
    audio: &AudioEngine,
) -> PlaybackTickPayload {
    let is_playing = audio.is_playing();
    let active_sound_id = audio.active_sound_id();
    let duration_secs = if is_playing {
        audio.active_sound_duration_secs()
    } else if let Some(sel_id) = selected_sound_id.as_deref() {
        sounds
            .iter()
            .find(|s| s.id == sel_id)
            .map(|s| s.duration_secs)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    PlaybackTickPayload {
        active_sound_id,
        selected_sound_id,
        is_playing,
        progress: if is_playing {
            audio.playback_progress()
        } else {
            0.0
        },
        duration_secs,
        speed: audio.speed(),
        mic_enabled: audio.mic_enabled(),
        headphones_enabled: audio.headphones_enabled(),
    }
}

fn build_state_payload(
    sounds: &[SoundItem],
    selected_sound_id: Option<String>,
    theme: &ThemeConfig,
    audio: &AudioEngine,
    revision: u64,
) -> SyncStatePayload {
    let dtos = sounds
        .iter()
        .map(|s| {
            let has_thumb = s.image_path.as_ref().is_some_and(|p| p.exists());
            SyncSoundDto {
                id: s.id.clone(),
                title: s.title.clone(),
                duration: s.formatted_duration(),
                duration_secs: s.duration_secs,
                has_thumbnail: has_thumb,
                thumbnail_url: has_thumb.then(|| format!("/api/thumbnails/{}?v={revision}", s.id)),
            }
        })
        .collect();

    let tick = build_playback_tick(sounds, selected_sound_id, audio);

    SyncStatePayload {
        sounds: dtos,
        active_sound_id: tick.active_sound_id,
        selected_sound_id: tick.selected_sound_id,
        is_playing: tick.is_playing,
        progress: tick.progress,
        duration_secs: tick.duration_secs,
        speed: tick.speed,
        mic_enabled: tick.mic_enabled,
        headphones_enabled: tick.headphones_enabled,
        mic_volume: audio.mic_volume(),
        headphones_volume: audio.headphones_volume(),
        theme: SyncThemeDto::from_theme(theme),
        revision,
    }
}

/// Handle to the embedded Tokio + Axum Android/iOS companion synchronization server.
#[derive(Clone)]
pub struct SyncServerHandle {
    shared: Arc<SyncShared>,
}

impl SyncServerHandle {
    /// Starts the embedded Tokio + Axum server on a background thread.
    pub fn start(initial_config: &AppConfig, audio: AudioEngine) -> Self {
        let preferred_port = initial_config.audio.sync_port;
        let shared = Arc::new(SyncShared::new(initial_config, audio));
        let server_shared = Arc::clone(&shared);
        let (port_tx, port_rx) = mpsc::sync_channel::<u16>(1);

        let spawn_result = thread::Builder::new()
            .name("wawefx-sync-server".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(err) => {
                        tracing::error!("Failed to build Tokio runtime for sync server: {err}");
                        let _ = port_tx.send(0);
                        return;
                    }
                };

                rt.block_on(async move {
                    let listener = match bind_with_fallback(preferred_port).await {
                        Ok(l) => l,
                        Err(err) => {
                            tracing::warn!("Sync server failed to bind HTTP/WS port: {err}");
                            let _ = port_tx.send(0);
                            return;
                        }
                    };

                    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                    server_shared.bound_port.store(actual_port, Ordering::Release);
                    let _ = port_tx.send(actual_port);

                    // Background progress broadcaster (~8 Hz only while playing & clients connected).
                    let ticker_shared = Arc::clone(&server_shared);
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_millis(125));
                        let mut was_playing = false;
                        loop {
                            interval.tick().await;
                            let clients = ticker_shared.connected_clients.load(Ordering::Acquire);
                            let playing = ticker_shared.audio.is_playing();
                            if clients > 0 && (playing || was_playing) {
                                ticker_shared.broadcast_playback_tick();
                            }
                            was_playing = playing;
                        }
                    });

                    let app = build_router(server_shared);
                    if let Err(err) = axum::serve(listener, app).await {
                        tracing::warn!("Sync server terminated: {err}");
                    }
                });
            });

        if spawn_result.is_ok() {
            if let Ok(bound) = port_rx.recv_timeout(Duration::from_millis(800)) {
                shared.bound_port.store(bound, Ordering::Release);
            }
        } else {
            shared.bound_port.store(0, Ordering::Release);
        }

        Self { shared }
    }

    /// Registers the desktop `egui::Context` so incoming remote WebSocket commands wake the UI.
    pub fn register_egui_ctx(&self, ctx: egui::Context) {
        if let Ok(mut guard) = self.shared.egui_ctx.lock()
            && guard.is_none()
        {
            *guard = Some(ctx);
        }
    }

    /// Updates the shared snapshot from desktop state and emits either `SoundsListUpdated`
    /// (if sounds or theme changed) or a lightweight `PlaybackTick`.
    pub fn publish_state(&self, config: &AppConfig, _audio: &AudioEngine) {
        let mut structural_changed = false;

        if let Ok(mut sounds_guard) = self.shared.sounds.write() {
            if *sounds_guard != config.sounds {
                *sounds_guard = config.sounds.clone();
                structural_changed = true;
            }
        }
        if let Ok(mut sel_guard) = self.shared.selected_sound_id.write() {
            *sel_guard = config.selected_sound_id.clone();
        }
        if let Ok(mut theme_guard) = self.shared.theme.write() {
            if *theme_guard != config.theme {
                *theme_guard = config.theme.clone();
                structural_changed = true;
            }
        }

        if structural_changed {
            self.shared.broadcast_structural_update();
        } else {
            self.shared.broadcast_playback_tick();
        }
    }

    /// Invalidates cached HTTP thumbnail bytes when a sound's image changes or is removed.
    pub fn invalidate_thumbnail(&self, sound_id: &str) {
        if let Ok(mut guard) = self.shared.thumb_cache.write() {
            guard.remove(sound_id);
        }
    }

    /// Drains queued remote changes so the desktop UI can sync `AppConfig` and save to disk.
    pub fn poll_desktop_notices(&self) -> Vec<DesktopSyncNotice> {
        let Ok(mut guard) = self.shared.desktop_notices.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *guard)
    }

    /// Returns the actual TCP port bound by the Axum server (or `0` if bind failed).
    pub fn bound_port(&self) -> u16 {
        self.shared.bound_port.load(Ordering::Acquire)
    }

    /// Returns the local LAN IPv4 address for mobile pairing QR codes.
    pub fn local_ip(&self) -> String {
        local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    }

    /// Full HTTP URL encoded into the companion QR code.
    pub fn pairing_url(&self) -> String {
        let port = self.bound_port();
        let ip = self.local_ip();
        if port == 0 {
            format!("http://{ip}/")
        } else {
            format!("http://{ip}:{port}/")
        }
    }

    /// Number of currently connected mobile WebSocket clients.
    pub fn connected_clients(&self) -> usize {
        self.shared.connected_clients.load(Ordering::Acquire)
    }
}

async fn bind_with_fallback(preferred_port: u16) -> std::io::Result<TcpListener> {
    let preferred_addr = SocketAddr::from(([0, 0, 0, 0], preferred_port));
    match TcpListener::bind(preferred_addr).await {
        Ok(listener) => Ok(listener),
        Err(_) => {
            let ephemeral_addr = SocketAddr::from(([0, 0, 0, 0], 0));
            TcpListener::bind(ephemeral_addr).await
        }
    }
}

fn build_router(shared: Arc<SyncShared>) -> Router {
    Router::new()
        .route("/", get(serve_mobile_index))
        .route("/api/state", get(serve_api_state))
        .route("/api/thumbnails/{id}", get(serve_thumbnail))
        .route("/ws", get(ws_handler))
        .with_state(shared)
}

async fn serve_mobile_index() -> Html<&'static str> {
    Html(MOBILE_WEB_HTML)
}

async fn serve_api_state(State(shared): State<Arc<SyncShared>>) -> Response {
    let payload = shared
        .latest_state
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| {
            build_state_payload(&[], None, &ThemeConfig::default(), &shared.audio, 0)
        });

    match serde_json::to_string(&payload) {
        Ok(json) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn serve_thumbnail(
    AxumPath(id): AxumPath<String>,
    State(shared): State<Arc<SyncShared>>,
) -> Response {
    let image_path = {
        let Ok(sounds) = shared.sounds.read() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        sounds
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.image_path.clone())
    };

    let Some(path) = image_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Ok(cache) = shared.thumb_cache.read()
        && let Some(entry) = cache.get(&id)
        && entry.source_path == path
    {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, entry.content_type),
                (header::CACHE_CONTROL, "public, max-age=300"),
            ],
            entry.bytes.clone(),
        )
            .into_response();
    }

    let path_for_task = path.clone();
    let encoded = tokio::task::spawn_blocking(move || encode_resized_thumbnail(&path_for_task)).await;

    match encoded {
        Ok(Some((content_type, bytes))) => {
            if let Ok(mut cache) = shared.thumb_cache.write() {
                cache.insert(
                    id,
                    CachedThumbBytes {
                        source_path: path,
                        content_type,
                        bytes: bytes.clone(),
                    },
                );
            }
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, "public, max-age=300"),
                ],
                bytes,
            )
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn encode_resized_thumbnail(path: &Path) -> Option<(&'static str, Bytes)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());

    let reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let decoded = reader.decode().ok()?;
    let resized = decoded.thumbnail(256, 256);
    let mut cursor = Cursor::new(Vec::new());

    match ext.as_deref() {
        Some("jpg" | "jpeg") => {
            resized
                .to_rgb8()
                .write_to(&mut cursor, ImageFormat::Jpeg)
                .ok()?;
            Some(("image/jpeg", Bytes::from(cursor.into_inner())))
        }
        _ => {
            resized
                .to_rgba8()
                .write_to(&mut cursor, ImageFormat::Png)
                .ok()?;
            Some(("image/png", Bytes::from(cursor.into_inner())))
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(shared): State<Arc<SyncShared>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws_client(socket, shared))
}

struct ClientGuard {
    shared: Arc<SyncShared>,
}

impl ClientGuard {
    fn enter(shared: Arc<SyncShared>) -> Self {
        shared.connected_clients.fetch_add(1, Ordering::AcqRel);
        shared.request_desktop_repaint();
        Self { shared }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.shared.connected_clients.fetch_sub(1, Ordering::AcqRel);
        self.shared.request_desktop_repaint();
    }
}

async fn handle_ws_client(mut socket: WebSocket, shared: Arc<SyncShared>) {
    let _guard = ClientGuard::enter(Arc::clone(&shared));
    let mut rx = shared.broadcast_tx.subscribe();

    if let Ok(initial_json) = shared.latest_event_json()
        && socket
            .send(Message::Text(Utf8Bytes::from(initial_json)))
            .await
            .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_ws_command(&shared, text.as_str());
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(json_text) => {
                        if socket.send(Message::Text(Utf8Bytes::from(json_text))).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(latest) = shared.latest_event_json()
                            && socket.send(Message::Text(Utf8Bytes::from(latest))).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn handle_ws_command(shared: &SyncShared, raw_json: &str) {
    let cmd: RemoteCommand = match serde_json::from_str(raw_json) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("Ignored malformed WebSocket command '{raw_json}': {err}");
            return;
        }
    };

    match cmd {
        RemoteCommand::PlaySound { id } => {
            let item = shared
                .sounds
                .read()
                .ok()
                .and_then(|list| list.iter().find(|s| s.id == id).cloned());
            if let Some(sound) = item {
                if let Err(err) = shared.audio.play_item(&sound) {
                    tracing::warn!("Remote PlaySound({}) failed: {err}", sound.title);
                } else {
                    if let Ok(mut sel) = shared.selected_sound_id.write() {
                        *sel = Some(id.clone());
                    }
                    shared.push_desktop_notice(DesktopSyncNotice::SelectSound(id));
                    shared.broadcast_playback_tick();
                }
            }
        }
        RemoteCommand::Stop => {
            shared.audio.stop();
            shared.request_desktop_repaint();
            shared.broadcast_playback_tick();
        }
        RemoteCommand::Rewind => {
            shared.audio.rewind();
            shared.request_desktop_repaint();
            shared.broadcast_playback_tick();
        }
        RemoteCommand::FastForward => {
            shared.audio.fast_forward();
            shared.request_desktop_repaint();
            shared.broadcast_playback_tick();
        }
        RemoteCommand::Seek { progress } => {
            shared.audio.seek_to_progress(progress);
            shared.request_desktop_repaint();
            shared.broadcast_playback_tick();
        }
        RemoteCommand::SetPitch { speed } => {
            let clamped = speed.clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
            shared.audio.set_speed(clamped);
            shared.push_desktop_notice(DesktopSyncNotice::SetPitch(clamped));
            shared.broadcast_playback_tick();
        }
        RemoteCommand::ToggleMic => {
            let next = !shared.audio.mic_enabled();
            shared.audio.set_mic_enabled(next);
            shared.push_desktop_notice(DesktopSyncNotice::SetMicEnabled(next));
            shared.broadcast_playback_tick();
        }
        RemoteCommand::ToggleHeadphones => {
            let next = !shared.audio.headphones_enabled();
            shared.audio.set_headphones_enabled(next);
            shared.push_desktop_notice(DesktopSyncNotice::SetHeadphonesEnabled(next));
            shared.broadcast_playback_tick();
        }
    }
}

const MOBILE_WEB_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no" />
<meta name="apple-mobile-web-app-capable" content="yes" />
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent" />
<title>WaweFX Remote</title>
<style>
  :root {
    --bg-main: #F4EFE6;
    --bg-toolbar: #EAE2D3;
    --toolbar-divider: #CFC3B0;
    --icon-primary: #2B2520;
    --icon-secondary: #7A6F64;
    --gauge-track: #7A6F64;
    --accent: #B08968;
    --pad-bg: #EAE2D3;
    --pad-hover: #DFD5C3;
    --pad-text: #2B2520;
  }
  * {
    box-sizing: border-box;
    -webkit-tap-highlight-color: transparent;
    touch-action: manipulation;
  }
  html, body {
    margin: 0;
    padding: 0;
    height: 100%;
    font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg-main);
    color: var(--pad-text);
    overflow: hidden;
    touch-action: manipulation;
    -webkit-user-select: none;
    user-select: none;
  }
  button, .pad, .pad-card, .circle-btn, .icon-btn, .pitch-readout {
    touch-action: manipulation;
    -webkit-touch-callout: none;
    -webkit-user-select: none;
    user-select: none;
  }
  .app-shell {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    width: 100%;
  }
  .toolbar {
    background: var(--bg-toolbar);
    border-bottom: 2px solid var(--toolbar-divider);
    padding: max(8px, env(safe-area-inset-top)) 10px 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex-shrink: 0;
  }
  .toolbar-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    flex-wrap: wrap;
  }
  .btn-group {
    display: flex;
    align-items: center;
    gap: 7px;
  }
  .circle-btn {
    width: 34px;
    height: 34px;
    border-radius: 50%;
    border: none;
    background: var(--icon-primary);
    color: var(--bg-toolbar);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    transition: transform 0.08s ease, opacity 0.15s ease;
  }
  .circle-btn:active, .icon-btn:active { transform: scale(0.92); }
  .circle-btn.playing { background: var(--accent); }
  .icon-btn {
    height: 34px;
    min-width: 34px;
    padding: 0 7px;
    border-radius: 10px;
    border: none;
    background: transparent;
    color: var(--icon-secondary);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    position: relative;
    cursor: pointer;
  }
  .icon-btn.toggle-on {
    background: color-mix(in srgb, var(--accent) 22%, transparent);
    color: var(--icon-primary);
  }
  .icon-btn.toggle-muted {
    opacity: 0.45;
  }
  .icon-btn.toggle-muted::after {
    content: "";
    position: absolute;
    width: 24px;
    height: 2.4px;
    background: var(--accent);
    transform: rotate(-40deg);
    border-radius: 2px;
  }
  .conn-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: #9ca3af;
    display: inline-block;
    margin-left: 2px;
  }
  .conn-dot.online { background: #10b981; box-shadow: 0 0 0 3px rgba(16,185,129,0.22); }
  .seek-row {
    display: flex;
    align-items: center;
    gap: 9px;
  }
  .seek-wrap {
    flex: 1;
    display: flex;
    align-items: center;
  }
  .seek-slider {
    -webkit-appearance: none;
    appearance: none;
    width: 100%;
    height: 5px;
    border-radius: 999px;
    background: var(--gauge-track);
    outline: none;
    touch-action: none;
  }
  .seek-slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--accent);
    border: none;
    cursor: pointer;
    box-shadow: 0 1px 4px rgba(0,0,0,0.3);
  }
  .seek-slider::-moz-range-thumb {
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--accent);
    border: none;
    cursor: pointer;
  }
  .time-label {
    font-size: 0.72rem;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    color: var(--pad-text);
    white-space: nowrap;
  }
  .workspace {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: row;
  }
  .grid-pane {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    -webkit-overflow-scrolling: touch;
    padding: 12px;
    touch-action: pan-y;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(118px, 1fr));
    gap: 10px;
  }
  .pad-card {
    background: var(--pad-bg);
    border: 2px solid rgba(0,0,0,0.12);
    border-radius: 11px;
    overflow: hidden;
    padding: 0;
    display: flex;
    flex-direction: column;
    color: var(--pad-text);
    text-align: left;
    cursor: pointer;
    position: relative;
    transition: transform 0.08s ease, background 0.15s ease, border-color 0.15s ease;
  }
  .pad-card:active { transform: scale(0.96); background: var(--pad-hover); }
  .pad-card.playing {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 34%, transparent);
  }
  .pad-media {
    width: 100%;
    aspect-ratio: 1 / 1;
    background: rgba(0,0,0,0.12);
    display: flex;
    align-items: center;
    justify-content: center;
    position: relative;
    overflow: hidden;
  }
  .pad-media img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
    pointer-events: none;
  }
  .badge {
    position: absolute;
    top: 6px;
    right: 6px;
    background: var(--accent);
    color: #fff;
    font-size: 0.62rem;
    font-weight: 800;
    padding: 2px 6px;
    border-radius: 4px;
  }
  .pad-meta {
    padding: 7px 8px 8px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .pad-title {
    font-size: 0.82rem;
    font-weight: 700;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pad-dur {
    font-size: 0.68rem;
    opacity: 0.78;
  }
  .pad-prog {
    height: 3px;
    background: rgba(0,0,0,0.2);
    width: 100%;
  }
  .pad-prog-fill {
    height: 100%;
    background: var(--accent);
    width: 0%;
  }
  .pitch-rail {
    width: 52px;
    flex-shrink: 0;
    border-left: 1px solid rgba(0,0,0,0.12);
    padding: 12px 4px max(12px, env(safe-area-inset-bottom));
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .pitch-caption {
    font-size: 0.65rem;
    font-weight: 800;
    letter-spacing: 0.5px;
    opacity: 0.78;
  }
  .pitch-track-area {
    flex: 1;
    width: 38px;
    min-height: 120px;
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    touch-action: none;
    cursor: pointer;
  }
  .pitch-track {
    width: 4px;
    height: 100%;
    border-radius: 999px;
    background: var(--gauge-track);
    position: relative;
  }
  .pitch-tick {
    position: absolute;
    left: 50%;
    width: 16px;
    height: 2px;
    background: var(--gauge-track);
    opacity: 0.7;
    transform: translate(-50%, 50%);
    bottom: 33.333%;
  }
  .pitch-thumb {
    position: absolute;
    left: 50%;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: var(--accent);
    transform: translate(-50%, 50%);
    box-shadow: 0 1px 4px rgba(0,0,0,0.3);
    pointer-events: none;
  }
  .pitch-readout {
    border: none;
    background: rgba(0,0,0,0.08);
    color: var(--pad-text);
    border-radius: 6px;
    padding: 4px 5px;
    font-size: 0.7rem;
    font-weight: 800;
    font-variant-numeric: tabular-nums;
    cursor: pointer;
  }
  .empty {
    text-align: center;
    padding: 42px 14px;
    font-size: 0.9rem;
    opacity: 0.8;
  }
</style>
</head>
<body>
  <div class="app-shell">
    <header class="toolbar">
      <div class="toolbar-top">
        <div class="btn-group">
          <button id="playBtn" class="circle-btn" type="button" aria-label="Play Selected">
            <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor"><polygon points="4,2 14,8 4,14"/></svg>
          </button>
          <button id="stopBtn" class="circle-btn" type="button" aria-label="Stop Playback">
            <svg width="13" height="13" viewBox="0 0 16 16" fill="currentColor"><rect x="3" y="3" width="10" height="10" rx="1.5"/></svg>
          </button>
          <button id="rwBtn" class="icon-btn" type="button" aria-label="Rewind 5s">
            <svg width="20" height="14" viewBox="0 0 24 16" fill="currentColor"><polygon points="11,2 2,8 11,14"/><polygon points="21,2 12,8 21,14"/></svg>
          </button>
          <button id="ffBtn" class="icon-btn" type="button" aria-label="Fast Forward 5s">
            <svg width="20" height="14" viewBox="0 0 24 16" fill="currentColor"><polygon points="3,2 12,8 3,14"/><polygon points="13,2 22,8 13,14"/></svg>
          </button>
        </div>
        <div class="btn-group">
          <button id="micBtn" class="icon-btn" type="button" aria-label="Toggle Mic">
            <svg width="18" height="22" viewBox="0 0 20 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
              <rect x="6.5" y="2" width="7" height="12" rx="3.5" fill="currentColor" stroke="none"/>
              <path d="M3.5 11.5a6.5 6.5 0 0 0 13 0"/>
              <line x1="10" y1="18" x2="10" y2="22"/>
              <line x1="5" y1="22" x2="15" y2="22"/>
            </svg>
          </button>
          <button id="hpBtn" class="icon-btn" type="button" aria-label="Toggle Headphones">
            <svg width="22" height="20" viewBox="0 0 24 22" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
              <polygon points="2,8 6,8 12,3 12,19 6,14 2,14" fill="currentColor" stroke="none"/>
              <path d="M15.5 7.5a5 5 0 0 1 0 7"/>
              <path d="M18.5 4.8a8.5 8.5 0 0 1 0 12.4"/>
            </svg>
          </button>
          <span id="connDot" class="conn-dot" title="Disconnected"></span>
        </div>
      </div>

      <div class="seek-row">
        <div class="seek-wrap">
          <input id="seekSlider" class="seek-slider" type="range" min="0" max="1000" value="0" aria-label="Playback Timeline" />
        </div>
        <span id="timeLabel" class="time-label">00:00 / 00:00</span>
      </div>
    </header>

    <div class="workspace">
      <main class="grid-pane">
        <div id="soundGrid" class="grid"></div>
        <div id="emptyState" class="empty" hidden>No sounds added on PC yet. Click + in WaweFX desktop to add sounds.</div>
      </main>

      <aside class="pitch-rail" aria-label="Pitch Control">
        <span class="pitch-caption">PITCH</span>
        <div id="pitchTrackArea" class="pitch-track-area">
          <div class="pitch-track">
            <div class="pitch-tick"></div>
            <div id="pitchThumb" class="pitch-thumb" style="bottom: 33.333%;"></div>
          </div>
        </div>
        <button id="pitchReadout" class="pitch-readout" type="button" title="Tap to reset 1.00x">1.00x</button>
      </aside>
    </div>
  </div>
<script>
(() => {
  const playBtn = document.getElementById('playBtn');
  const stopBtn = document.getElementById('stopBtn');
  const rwBtn = document.getElementById('rwBtn');
  const ffBtn = document.getElementById('ffBtn');
  const micBtn = document.getElementById('micBtn');
  const hpBtn = document.getElementById('hpBtn');
  const connDot = document.getElementById('connDot');
  const seekSlider = document.getElementById('seekSlider');
  const timeLabel = document.getElementById('timeLabel');
  const grid = document.getElementById('soundGrid');
  const emptyState = document.getElementById('emptyState');
  const pitchTrackArea = document.getElementById('pitchTrackArea');
  const pitchThumb = document.getElementById('pitchThumb');
  const pitchReadout = document.getElementById('pitchReadout');

  let ws = null;
  let reconnectTimer = null;
  let isSeeking = false;
  let isDraggingPitch = false;
  let pitchTimer = null;
  let currentSounds = [];
  let selectedSoundId = null;
  let lastGridSignature = '';
  const padMap = new Map();

  // iOS Safari double-tap zoom guard
  let lastGlobalTouchEnd = 0;
  document.addEventListener('gesturestart', (e) => e.preventDefault(), { passive: false });
  document.addEventListener('touchend', (e) => {
    const now = Date.now();
    if (now - lastGlobalTouchEnd <= 320) {
      const tag = (e.target && e.target.tagName) ? e.target.tagName.toUpperCase() : '';
      if (tag !== 'INPUT') {
        e.preventDefault();
      }
    }
    lastGlobalTouchEnd = now;
  }, { passive: false });

  function bindFastTap(el, handler) {
    let startX = 0;
    let startY = 0;
    let moved = false;
    let lastTouchTs = 0;

    el.addEventListener('touchstart', (ev) => {
      if (ev.touches.length !== 1) return;
      startX = ev.touches[0].clientX;
      startY = ev.touches[0].clientY;
      moved = false;
    }, { passive: true });

    el.addEventListener('touchmove', (ev) => {
      if (ev.touches.length !== 1) return;
      const dx = ev.touches[0].clientX - startX;
      const dy = ev.touches[0].clientY - startY;
      if (dx * dx + dy * dy > 100) {
        moved = true;
      }
    }, { passive: true });

    el.addEventListener('touchend', (ev) => {
      if (moved) return;
      ev.preventDefault();
      lastTouchTs = Date.now();
      handler();
    }, { passive: false });

    el.addEventListener('click', (ev) => {
      if (Date.now() - lastTouchTs < 500) {
        ev.preventDefault();
        return;
      }
      handler();
    });
  }

  function sendCmd(obj) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(obj));
    }
  }

  function fmtTime(secs) {
    const t = Math.max(0, Math.round(Number(secs || 0)));
    const m = String(Math.floor(t / 60)).padStart(2, '0');
    const s = String(t % 60).padStart(2, '0');
    return m + ':' + s;
  }

  function applyTheme(t) {
    if (!t) return;
    const root = document.documentElement.style;
    root.setProperty('--bg-main', t.bg_main);
    root.setProperty('--bg-toolbar', t.bg_toolbar);
    root.setProperty('--toolbar-divider', t.toolbar_divider);
    root.setProperty('--icon-primary', t.icon_primary);
    root.setProperty('--icon-secondary', t.icon_secondary);
    root.setProperty('--gauge-track', t.gauge_and_track);
    root.setProperty('--accent', t.accent_color);
    root.setProperty('--pad-bg', t.pad_card_bg);
    root.setProperty('--pad-hover', t.pad_card_hover);
    root.setProperty('--pad-text', t.pad_text);
  }

  function updatePitchVisual(speed) {
    const clamped = Math.min(2.0, Math.max(0.5, Number(speed || 1.0)));
    const pct = ((clamped - 0.5) / 1.5) * 100;
    pitchThumb.style.bottom = pct.toFixed(2) + '%';
    pitchReadout.textContent = clamped.toFixed(2) + 'x';
  }

  let lastDur = 0;
  function updateSeekVisual(tick) {
    const dur = Number(tick.duration_secs || 0);
    lastDur = dur;
    const prog = tick.is_playing ? Math.min(1, Math.max(0, Number(tick.progress || 0))) : 0;
    if (!isSeeking) {
      seekSlider.value = String(Math.round(prog * 1000));
      const pct = (prog * 100).toFixed(2) + '%';
      seekSlider.style.background = `linear-gradient(to right, var(--accent) 0%, var(--accent) ${pct}, var(--gauge-track) ${pct}, var(--gauge-track) 100%)`;
      timeLabel.textContent = fmtTime(dur * prog) + ' / ' + fmtTime(dur);
    }
  }

  function waveSvg() {
    return '<svg width="46" height="46" viewBox="0 0 48 48" fill="none" stroke="currentColor" stroke-width="3.2" stroke-linecap="round">' +
      '<line x1="10" y1="20" x2="10" y2="28"/>' +
      '<line x1="17" y1="14" x2="17" y2="34"/>' +
      '<line x1="24" y1="9" x2="24" y2="39"/>' +
      '<line x1="31" y1="14" x2="31" y2="34"/>' +
      '<line x1="38" y1="20" x2="38" y2="28"/></svg>';
  }

  function createSoundCard(s) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'pad-card';
    btn.dataset.id = s.id;
    bindFastTap(btn, () => sendCmd({ cmd: 'PlaySound', id: s.id }));

    const media = document.createElement('div');
    media.className = 'pad-media';

    let imgEl = null;
    if (s.thumbnail_url) {
      imgEl = document.createElement('img');
      imgEl.src = s.thumbnail_url;
      imgEl.alt = s.title;
      imgEl.decoding = 'async';
      imgEl.onerror = () => { media.innerHTML = waveSvg(); media.appendChild(badge); };
      media.appendChild(imgEl);
    } else {
      media.innerHTML = waveSvg();
    }

    const badge = document.createElement('span');
    badge.className = 'badge';
    badge.textContent = 'PLAYING';
    badge.hidden = true;
    media.appendChild(badge);

    const meta = document.createElement('div');
    meta.className = 'pad-meta';
    const titleEl = document.createElement('div');
    titleEl.className = 'pad-title';
    titleEl.textContent = s.title;
    const durEl = document.createElement('div');
    durEl.className = 'pad-dur';
    durEl.textContent = s.duration;
    meta.appendChild(titleEl);
    meta.appendChild(durEl);

    const prog = document.createElement('div');
    prog.className = 'pad-prog';
    prog.hidden = true;
    const progFill = document.createElement('div');
    progFill.className = 'pad-prog-fill';
    prog.appendChild(progFill);

    btn.appendChild(media);
    btn.appendChild(meta);
    btn.appendChild(prog);

    return {
      card: btn,
      media,
      imgEl,
      titleEl,
      durEl,
      badge,
      prog,
      progFill,
      thumbUrl: s.thumbnail_url || '',
    };
  }

  function syncSoundCards(sounds) {
    currentSounds = sounds || [];
    emptyState.hidden = currentSounds.length > 0;
    const sig = currentSounds
      .map((s) => s.id + '|' + s.title + '|' + s.duration + '|' + (s.thumbnail_url || ''))
      .join(';');
    if (sig === lastGridSignature) return;
    lastGridSignature = sig;

    const nextIds = new Set(currentSounds.map((s) => s.id));
    for (const [id, entry] of padMap.entries()) {
      if (!nextIds.has(id)) {
        entry.card.remove();
        padMap.delete(id);
      }
    }

    // Reorder / create / update cards in place without touching unchanged <img> elements.
    for (const s of currentSounds) {
      let entry = padMap.get(s.id);
      const nextThumb = s.thumbnail_url || '';
      if (!entry || entry.thumbUrl !== nextThumb) {
        if (entry) entry.card.remove();
        entry = createSoundCard(s);
        padMap.set(s.id, entry);
      } else {
        if (entry.titleEl.textContent !== s.title) entry.titleEl.textContent = s.title;
        if (entry.durEl.textContent !== s.duration) entry.durEl.textContent = s.duration;
      }
      grid.appendChild(entry.card);
    }
  }

  function applyPlaybackTick(tick) {
    selectedSoundId = tick.selected_sound_id || null;
    playBtn.classList.toggle('playing', Boolean(tick.is_playing));
    micBtn.className = 'icon-btn ' + (tick.mic_enabled ? 'toggle-on' : 'toggle-muted');
    hpBtn.className = 'icon-btn ' + (tick.headphones_enabled ? 'toggle-on' : 'toggle-muted');

    updateSeekVisual(tick);
    if (!isDraggingPitch) {
      updatePitchVisual(tick.speed);
    }

    const activeId = tick.is_playing ? tick.active_sound_id : null;
    const pct = (Math.min(1, Math.max(0, Number(tick.progress || 0))) * 100).toFixed(1) + '%';
    for (const [id, entry] of padMap.entries()) {
      const isThis = Boolean(activeId && id === activeId);
      entry.card.classList.toggle('playing', isThis);
      entry.badge.hidden = !isThis;
      entry.prog.hidden = !isThis;
      if (isThis) {
        entry.progFill.style.width = pct;
      }
    }
  }

  bindFastTap(playBtn, () => {
    const targetId = selectedSoundId || (currentSounds[0] && currentSounds[0].id);
    if (targetId) sendCmd({ cmd: 'PlaySound', id: targetId });
  });
  bindFastTap(stopBtn, () => sendCmd({ cmd: 'Stop' }));
  bindFastTap(rwBtn, () => sendCmd({ cmd: 'Rewind' }));
  bindFastTap(ffBtn, () => sendCmd({ cmd: 'FastForward' }));
  bindFastTap(micBtn, () => sendCmd({ cmd: 'ToggleMic' }));
  bindFastTap(hpBtn, () => sendCmd({ cmd: 'ToggleHeadphones' }));

  seekSlider.addEventListener('pointerdown', () => { isSeeking = true; });
  seekSlider.addEventListener('pointerup', () => { isSeeking = false; });
  seekSlider.addEventListener('input', () => {
    const p = Number(seekSlider.value) / 1000;
    const pct = (p * 100).toFixed(2) + '%';
    seekSlider.style.background = `linear-gradient(to right, var(--accent) 0%, var(--accent) ${pct}, var(--gauge-track) ${pct}, var(--gauge-track) 100%)`;
    timeLabel.textContent = fmtTime(lastDur * p) + ' / ' + fmtTime(lastDur);
  });
  seekSlider.addEventListener('change', () => {
    isSeeking = false;
    sendCmd({ cmd: 'Seek', progress: Number(seekSlider.value) / 1000 });
  });

  function pitchFromClientY(clientY) {
    const rect = pitchTrackArea.getBoundingClientRect();
    const norm = Math.min(1, Math.max(0, (rect.bottom - clientY) / Math.max(1, rect.height)));
    const speed = 0.5 + norm * 1.5;
    updatePitchVisual(speed);
    if (!pitchTimer) {
      pitchTimer = setTimeout(() => {
        pitchTimer = null;
        const txt = parseFloat(pitchReadout.textContent);
        if (!Number.isNaN(txt)) sendCmd({ cmd: 'SetPitch', speed: txt });
      }, 40);
    }
  }

  pitchTrackArea.addEventListener('pointerdown', (ev) => {
    isDraggingPitch = true;
    pitchTrackArea.setPointerCapture(ev.pointerId);
    pitchFromClientY(ev.clientY);
  });
  pitchTrackArea.addEventListener('pointermove', (ev) => {
    if (isDraggingPitch) pitchFromClientY(ev.clientY);
  });
  function finishPitchDrag(ev) {
    if (!isDraggingPitch) return;
    isDraggingPitch = false;
    pitchFromClientY(ev.clientY);
    const val = parseFloat(pitchReadout.textContent);
    if (!Number.isNaN(val)) sendCmd({ cmd: 'SetPitch', speed: val });
  }
  pitchTrackArea.addEventListener('pointerup', finishPitchDrag);
  pitchTrackArea.addEventListener('pointercancel', () => { isDraggingPitch = false; });
  bindFastTap(pitchReadout, () => {
    updatePitchVisual(1.0);
    sendCmd({ cmd: 'SetPitch', speed: 1.0 });
  });

  function scheduleReconnect() {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, 1200);
  }

  function connect() {
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    ws = new WebSocket(proto + location.host + '/ws');
    ws.onopen = () => {
      connDot.classList.add('online');
      connDot.title = 'Connected';
    };
    ws.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        if (!msg || !msg.payload) return;
        if (msg.type === 'StateUpdated') {
          applyTheme(msg.payload.theme);
          syncSoundCards(msg.payload.sounds);
          applyPlaybackTick(msg.payload);
        } else if (msg.type === 'SoundsListUpdated') {
          applyTheme(msg.payload.theme);
          syncSoundCards(msg.payload.sounds);
        } else if (msg.type === 'PlaybackTick') {
          applyPlaybackTick(msg.payload);
        }
      } catch (_) {}
    };
    ws.onclose = () => {
      connDot.classList.remove('online');
      connDot.title = 'Reconnecting...';
      scheduleReconnect();
    };
    ws.onerror = () => {
      try { ws.close(); } catch (_) {}
    };
  }

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') connect();
  });
  window.addEventListener('pageshow', connect);
  window.addEventListener('online', connect);

  connect();
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_command_deserializes_all_variants() {
        let play: RemoteCommand =
            serde_json::from_str(r#"{"cmd":"PlaySound","id":"abc-123"}"#).unwrap();
        assert_eq!(
            play,
            RemoteCommand::PlaySound {
                id: "abc-123".to_string()
            }
        );

        let stop: RemoteCommand = serde_json::from_str(r#"{"cmd":"Stop"}"#).unwrap();
        assert_eq!(stop, RemoteCommand::Stop);

        let seek: RemoteCommand =
            serde_json::from_str(r#"{"cmd":"Seek","progress":0.42}"#).unwrap();
        assert_eq!(seek, RemoteCommand::Seek { progress: 0.42 });

        let pitch: RemoteCommand =
            serde_json::from_str(r#"{"cmd":"SetPitch","speed":1.45}"#).unwrap();
        assert_eq!(pitch, RemoteCommand::SetPitch { speed: 1.45 });

        let pitch_alias: RemoteCommand =
            serde_json::from_str(r#"{"cmd":"SetPitch","value":0.75}"#).unwrap();
        assert_eq!(pitch_alias, RemoteCommand::SetPitch { speed: 0.75 });

        let mic: RemoteCommand = serde_json::from_str(r#"{"cmd":"ToggleMic"}"#).unwrap();
        assert_eq!(mic, RemoteCommand::ToggleMic);

        let hp: RemoteCommand = serde_json::from_str(r#"{"cmd":"ToggleHeadphones"}"#).unwrap();
        assert_eq!(hp, RemoteCommand::ToggleHeadphones);
    }

    #[test]
    fn sync_events_separate_structural_updates_from_playback_ticks() {
        let theme = ThemeConfig::default();
        let list_ev = SyncEvent::SoundsListUpdated(SoundsListPayload {
            sounds: vec![SyncSoundDto {
                id: "1".to_string(),
                title: "Horn".to_string(),
                duration: "00:03".to_string(),
                duration_secs: 3.0,
                has_thumbnail: true,
                thumbnail_url: Some("/api/thumbnails/1?v=2".to_string()),
            }],
            theme: SyncThemeDto::from_theme(&theme),
            revision: 2,
        });
        let list_json = serde_json::to_string(&list_ev).unwrap();
        assert!(list_json.contains(r#""type":"SoundsListUpdated""#));
        assert!(list_json.contains(r##""bg_main":"#F4EFE6""##));
        assert!(list_json.contains(r##""pad_card_bg":"#EAE2D3""##));
        assert!(list_json.contains(r##""accent_color":"#B08968""##));

        let tick_ev = SyncEvent::PlaybackTick(PlaybackTickPayload {
            active_sound_id: Some("1".to_string()),
            selected_sound_id: Some("1".to_string()),
            is_playing: true,
            progress: 0.5,
            duration_secs: 3.0,
            speed: 1.25,
            mic_enabled: true,
            headphones_enabled: false,
        });
        let tick_json = serde_json::to_string(&tick_ev).unwrap();
        assert!(tick_json.contains(r#""type":"PlaybackTick""#));
        assert!(tick_json.contains(r#""progress":0.5"#));
        assert!(!tick_json.contains("thumbnail_url"));
    }

    #[test]
    fn encodes_resized_png_thumbnail_bytes() {
        let temp_img =
            std::env::temp_dir().join(format!("wawefx_thumb_{}.png", uuid::Uuid::new_v4()));
        let img = image::RgbaImage::from_pixel(512, 512, image::Rgba([255, 127, 80, 255]));
        img.save(&temp_img).expect("save temp png");

        let encoded = encode_resized_thumbnail(&temp_img);
        let _ = std::fs::remove_file(&temp_img);

        let (ctype, bytes) = encoded.expect("thumbnail encoded");
        assert_eq!(ctype, "image/png");
        assert!(!bytes.is_empty());
    }
}
