pub mod apo_installer;
pub mod mixer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;

use crate::config::{AppConfig, AudioRoutingConfig, SoundItem, sanitize_trim_range};

/// Special sound ID used while auditioning trimmed slices inside the sound editor modal.
pub const PREVIEW_SOUND_ID: &str = "__wawefx_trim_preview__";

/// Decoded and normalized stereo 48 kHz PCM audio buffer held in memory.
#[derive(Debug, Clone)]
pub struct DecodedSoundBuffer {
    /// Interleaved stereo (`L, R`) samples normalized to [`crate::config::TARGET_SAMPLE_RATE`].
    pub samples: Arc<Vec<f32>>,
    /// Number of stereo frames (`samples.len() / 2`).
    pub frame_count: usize,
    /// Duration of the normalized buffer in seconds at 1.0x speed.
    pub duration_secs: f32,
}

/// Immutable handle to the currently playing sound passed to audio callbacks.
#[derive(Debug, Clone)]
pub struct ActiveSoundData {
    pub id: String,
    pub title: String,
    pub samples: Arc<Vec<f32>>,
    pub frame_count: usize,
    pub trim_start_frame: usize,
    pub trim_end_frame: usize,
    pub volume_trim: f32,
}

impl ActiveSoundData {
    /// Returns clamped `[start_frame, end_frame)` playback bounds with at least 1 frame when non-empty.
    pub fn effective_frame_bounds(&self) -> (usize, usize) {
        let total = self.frame_count.min(self.samples.len() / 2);
        if total == 0 {
            return (0, 0);
        }
        let raw_end = if self.trim_end_frame == 0 {
            total
        } else {
            self.trim_end_frame.min(total)
        };
        let start = self.trim_start_frame.min(total.saturating_sub(1));
        let end = raw_end.max(start + 1).min(total);
        (start, end)
    }

    /// Returns the number of active frames in the `[trim_start_frame, trim_end_frame)` slice.
    pub fn trimmed_frame_count(&self) -> usize {
        let (start, end) = self.effective_frame_bounds();
        end.saturating_sub(start)
    }
}

/// Converts normalized `trim_start..trim_end` fractions into `[start_frame, end_frame)` frame bounds.
pub fn compute_trim_frame_bounds(
    frame_count: usize,
    trim_start: f32,
    trim_end: f32,
) -> (usize, usize) {
    if frame_count == 0 {
        return (0, 0);
    }
    let (norm_s, norm_e) = sanitize_trim_range(trim_start, trim_end);
    let start = ((norm_s as f64 * frame_count as f64).round() as usize)
        .min(frame_count.saturating_sub(1));
    let end = ((norm_e as f64 * frame_count as f64).round() as usize)
        .max(start + 1)
        .min(frame_count);
    (start, end)
}

pub type ActiveSound = Arc<ActiveSoundData>;

/// Enumerated OS audio input and output device names.
#[derive(Debug, Clone, Default)]
pub struct AudioDeviceList {
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,
}

/// Diagnostic snapshot exposed to the toolbar `!` status indicator and Settings modal.
#[derive(Debug, Clone, Default)]
pub struct AudioEngineStatus {
    pub backend_name: &'static str,
    pub has_warning: bool,
    pub summary: String,
    pub warnings: Vec<String>,
    pub active_mic_input: Option<String>,
    pub active_virtual_mic_output: Option<String>,
    pub active_headphones_output: Option<String>,
    pub mic_ring_fill_ms: f32,
    pub measured_mic_in_fps: u32,
    pub measured_vmic_out_fps: u32,
}

/// Abstraction over WaweFX audio routing backends (Software Mixer vs. future Windows APO hook).
pub trait AudioBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn apply_routing_config(&self, config: &AudioRoutingConfig);
    fn play_sound(&self, sound: ActiveSound);
    fn stop(&self);
    fn seek_relative_secs(&self, delta_secs: f32);
    fn seek_to_progress(&self, progress: f32);
    fn set_scrub_paused(&self, paused: bool);
    fn set_speed(&self, speed: f32);
    fn speed(&self) -> f32;
    fn set_mic_volume(&self, volume: f32);
    fn mic_volume(&self) -> f32;
    fn set_headphones_volume(&self, volume: f32);
    fn headphones_volume(&self) -> f32;
    fn set_mic_enabled(&self, enabled: bool);
    fn mic_enabled(&self) -> bool;
    fn set_headphones_enabled(&self, enabled: bool);
    fn headphones_enabled(&self) -> bool;
    fn set_allow_overlap(&self, allow: bool);
    fn allow_overlap(&self) -> bool;
    fn is_playing(&self) -> bool;
    fn playback_progress(&self) -> f32;
    fn active_sound_duration_secs(&self) -> f32;
    fn active_sound_id(&self) -> Option<String>;
    fn status_snapshot(&self) -> AudioEngineStatus;
    fn available_devices(&self) -> AudioDeviceList;
}

/// High-level WaweFX audio controller coordinating decoded buffers and backend routing.
#[derive(Clone)]
pub struct AudioEngine {
    backend: Arc<dyn AudioBackend>,
    decoded_cache: Arc<RwLock<HashMap<String, DecodedSoundBuffer>>>,
    waveform_cache: Arc<RwLock<HashMap<PathBuf, Arc<Vec<f32>>>>>,
    last_preview_buffer: Arc<Mutex<Option<(PathBuf, DecodedSoundBuffer)>>>,
}

impl AudioEngine {
    /// Initializes the default real-time software mixer backend with the persisted routing config.
    pub fn new(config: &AudioRoutingConfig) -> Self {
        let backend: Arc<dyn AudioBackend> = Arc::new(mixer::SoftwareMixerBackend::new(config));
        Self::with_backend(backend, config)
    }

    /// Wraps a specific [`AudioBackend`] implementation and applies the initial routing config.
    pub fn with_backend(backend: Arc<dyn AudioBackend>, config: &AudioRoutingConfig) -> Self {
        backend.apply_routing_config(config);
        Self {
            backend,
            decoded_cache: Arc::new(RwLock::new(HashMap::new())),
            waveform_cache: Arc::new(RwLock::new(HashMap::new())),
            last_preview_buffer: Arc::new(Mutex::new(None)),
        }
    }

    /// Probes an audio file's duration (seconds) by decoding and normalizing offline.
    pub fn probe_audio_duration(path: &Path) -> Result<f32> {
        let decoded = mixer::decode_audio_file(path)?;
        Ok(decoded.duration_secs)
    }

    /// Probes an audio file's duration and downsampled waveform peaks, caching both peaks and preview PCM.
    pub fn probe_audio_with_peaks(&self, path: &Path) -> Result<(f32, Arc<Vec<f32>>)> {
        let resolved = AppConfig::resolve_asset_path(path);
        let decoded = mixer::decode_audio_file(&resolved)?;
        let duration = decoded.duration_secs;
        let peaks = Arc::new(mixer::compute_waveform_peaks(
            &decoded.samples,
            mixer::WAVEFORM_BAR_COUNT,
        ));
        if let Ok(mut wf_guard) = self.waveform_cache.write() {
            wf_guard.insert(resolved.clone(), Arc::clone(&peaks));
        }
        if let Ok(mut prev_guard) = self.last_preview_buffer.lock() {
            *prev_guard = Some((resolved, decoded));
        }
        Ok((duration, peaks))
    }

    /// Returns cached downsampled waveform peaks for `path`, decoding once if not cached yet.
    pub fn waveform_peaks_for_path(&self, path: &Path) -> Result<Arc<Vec<f32>>> {
        let resolved = AppConfig::resolve_asset_path(path);
        if let Some(cached) = self
            .waveform_cache
            .read()
            .ok()
            .and_then(|guard| guard.get(&resolved).cloned())
        {
            return Ok(cached);
        }
        let (_, peaks) = self.probe_audio_with_peaks(&resolved)?;
        Ok(peaks)
    }

    /// Previews a trimmed `[trim_start, trim_end]` slice directly from `path` inside the editor modal.
    pub fn preview_trimmed_file(
        &self,
        path: &Path,
        volume_trim: f32,
        trim_start: f32,
        trim_end: f32,
    ) -> Result<()> {
        let resolved = AppConfig::resolve_asset_path(path);
        let buffer = if let Some(cached) = self
            .last_preview_buffer
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .filter(|(p, _)| p == &resolved)
                    .map(|(_, b)| b.clone())
            }) {
            cached
        } else {
            let decoded = mixer::decode_audio_file(&resolved)?;
            if let Ok(mut prev_guard) = self.last_preview_buffer.lock() {
                *prev_guard = Some((resolved.clone(), decoded.clone()));
            }
            decoded
        };

        let (trim_start_frame, trim_end_frame) =
            compute_trim_frame_bounds(buffer.frame_count, trim_start, trim_end);
        let active = Arc::new(ActiveSoundData {
            id: PREVIEW_SOUND_ID.to_string(),
            title: "Trim Preview".to_string(),
            samples: buffer.samples,
            frame_count: buffer.frame_count,
            trim_start_frame,
            trim_end_frame,
            volume_trim: volume_trim.clamp(0.0, 2.0),
        });
        self.stop();
        self.backend.play_sound(active);
        Ok(())
    }

    /// Decodes a [`SoundItem`] offline into a 48 kHz stereo buffer and stores it in the cache.
    pub fn decode_and_cache(&self, sound: &SoundItem) -> Result<DecodedSoundBuffer> {
        let resolved_path = sound.resolved_audio_path();
        let decoded = mixer::decode_audio_file(&resolved_path)?;
        if let Ok(mut guard) = self.decoded_cache.write() {
            guard.insert(sound.id.clone(), decoded.clone());
        }
        let peaks = Arc::new(mixer::compute_waveform_peaks(
            &decoded.samples,
            mixer::WAVEFORM_BAR_COUNT,
        ));
        if let Ok(mut wf_guard) = self.waveform_cache.write() {
            wf_guard.entry(resolved_path).or_insert(peaks);
        }
        Ok(decoded)
    }

    /// Drops a cached sound buffer (for example after a sound is removed or its file changes).
    pub fn invalidate_cached_sound(&self, sound_id: &str) {
        if let Ok(mut guard) = self.decoded_cache.write() {
            guard.remove(sound_id);
        }
    }

    /// Plays a soundboard item immediately, decoding and caching on first play if needed.
    pub fn play_item(&self, sound: &SoundItem) -> Result<()> {
        let cached = self
            .decoded_cache
            .read()
            .ok()
            .and_then(|guard| guard.get(&sound.id).cloned());

        let buffer = match cached {
            Some(buf) => buf,
            None => self.decode_and_cache(sound)?,
        };

        let (trim_start_frame, trim_end_frame) =
            compute_trim_frame_bounds(buffer.frame_count, sound.trim_start, sound.trim_end);

        let active = Arc::new(ActiveSoundData {
            id: sound.id.clone(),
            title: sound.title.clone(),
            samples: buffer.samples,
            frame_count: buffer.frame_count,
            trim_start_frame,
            trim_end_frame,
            volume_trim: sound.volume_trim,
        });
        self.backend.play_sound(active);
        Ok(())
    }

    pub fn stop(&self) {
        self.backend.stop();
    }

    /// Rewinds active playback by 5 seconds.
    pub fn rewind(&self) {
        self.backend.seek_relative_secs(-5.0);
    }

    /// Fast-forwards active playback by 5 seconds.
    pub fn fast_forward(&self) {
        self.backend.seek_relative_secs(5.0);
    }

    /// Seeks directly to a normalized playback position in `0.0..=1.0`.
    pub fn seek_to_progress(&self, progress: f32) {
        self.backend.seek_to_progress(progress);
    }

    /// Mutes/pauses sound playback temporarily during timeline scrubbing without clearing the active sound.
    pub fn set_scrub_paused(&self, paused: bool) {
        self.backend.set_scrub_paused(paused);
    }

    pub fn apply_routing_config(&self, config: &AudioRoutingConfig) {
        self.backend.apply_routing_config(config);
    }

    pub fn set_speed(&self, speed: f32) {
        self.backend.set_speed(speed);
    }

    pub fn speed(&self) -> f32 {
        self.backend.speed()
    }

    pub fn set_mic_volume(&self, volume: f32) {
        self.backend.set_mic_volume(volume);
    }

    pub fn mic_volume(&self) -> f32 {
        self.backend.mic_volume()
    }

    pub fn set_headphones_volume(&self, volume: f32) {
        self.backend.set_headphones_volume(volume);
    }

    pub fn headphones_volume(&self) -> f32 {
        self.backend.headphones_volume()
    }

    pub fn set_mic_enabled(&self, enabled: bool) {
        self.backend.set_mic_enabled(enabled);
    }

    pub fn mic_enabled(&self) -> bool {
        self.backend.mic_enabled()
    }

    pub fn set_headphones_enabled(&self, enabled: bool) {
        self.backend.set_headphones_enabled(enabled);
    }

    pub fn headphones_enabled(&self) -> bool {
        self.backend.headphones_enabled()
    }

    pub fn set_allow_overlap(&self, allow: bool) {
        self.backend.set_allow_overlap(allow);
    }

    pub fn allow_overlap(&self) -> bool {
        self.backend.allow_overlap()
    }

    pub fn is_playing(&self) -> bool {
        self.backend.is_playing()
    }

    pub fn playback_progress(&self) -> f32 {
        self.backend.playback_progress()
    }

    pub fn active_sound_duration_secs(&self) -> f32 {
        self.backend.active_sound_duration_secs()
    }

    pub fn active_sound_id(&self) -> Option<String> {
        self.backend.active_sound_id()
    }

    pub fn status_snapshot(&self) -> AudioEngineStatus {
        self.backend.status_snapshot()
    }

    pub fn available_devices(&self) -> AudioDeviceList {
        self.backend.available_devices()
    }
}
