use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use rodio::Source;

use crate::audio::{
    ActiveSound, AudioBackend, AudioDeviceList, AudioEngineStatus, DecodedSoundBuffer,
};
use crate::config::{
    AudioRoutingConfig, MAX_PLAYBACK_SPEED, MAX_STREAM_VOLUME, MIN_PLAYBACK_SPEED,
    MIN_STREAM_VOLUME, TARGET_CHANNELS, TARGET_SAMPLE_RATE,
};

/// Power-of-two capacity (stereo frames) for the lock-free physical microphone ring buffer.
const MIC_RING_CAPACITY_FRAMES: usize = 16_384;
const MIC_RING_MASK: usize = MIC_RING_CAPACITY_FRAMES - 1;

const MIC_PREBUFFER_MS: usize = 25;
const MIC_MAX_LATENCY_MS: usize = 80;
const MIC_TARGET_LATENCY_MS: usize = 25;

const LEADER_FALLBACK: u8 = 0;
const LEADER_HEADPHONES: u8 = 1;
const LEADER_VIRTUAL_MIC: u8 = 2;

/// Pre-allocated, lock-free single-producer single-consumer (SPSC) stereo ring buffer.
///
/// Stores stereo `(L, R)` `f32` samples bit-cast into `AtomicU32` slots so neither the
/// physical microphone input callback nor the virtual microphone output callback ever blocks
/// or allocates on the heap.
pub struct LockFreeStereoRingBuffer {
    slots: Box<[AtomicU32]>,
    write_idx: AtomicUsize,
    read_idx: AtomicUsize,
}

impl LockFreeStereoRingBuffer {
    pub fn new() -> Self {
        let total_floats = MIC_RING_CAPACITY_FRAMES * TARGET_CHANNELS as usize;
        let slots = (0..total_floats)
            .map(|_| AtomicU32::new(0.0f32.to_bits()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            write_idx: AtomicUsize::new(0),
            read_idx: AtomicUsize::new(0),
        }
    }

    /// Clears buffered samples from the control thread when reconfiguring audio streams.
    pub fn clear(&self) {
        let w = self.write_idx.load(Ordering::Acquire);
        self.read_idx.store(w, Ordering::Release);
    }

    /// Returns the number of stereo frames currently available to read.
    #[inline]
    pub fn len_frames(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    /// Pushes one stereo frame into the ring buffer, overwriting nothing if full.
    #[inline]
    pub fn push_frame(&self, left: f32, right: f32) -> bool {
        let w = self.write_idx.load(Ordering::Relaxed);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= MIC_RING_CAPACITY_FRAMES {
            return false;
        }
        let base = (w & MIC_RING_MASK) * 2;
        self.slots[base].store(left.to_bits(), Ordering::Relaxed);
        self.slots[base + 1].store(right.to_bits(), Ordering::Relaxed);
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pops one stereo frame from the ring buffer, returning `None` when empty.
    #[inline]
    pub fn pop_frame(&self) -> Option<(f32, f32)> {
        let r = self.read_idx.load(Ordering::Relaxed);
        let w = self.write_idx.load(Ordering::Acquire);
        if r == w {
            return None;
        }
        let base = (r & MIC_RING_MASK) * 2;
        let left = f32::from_bits(self.slots[base].load(Ordering::Relaxed));
        let right = f32::from_bits(self.slots[base + 1].load(Ordering::Relaxed));
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        Some((left, right))
    }

    /// Trims excess buffered frames on the consumer side to bound real-time mic passthrough latency.
    #[inline]
    pub fn trim_excess_latency(&self, mic_sample_rate: u32) {
        let rate = mic_sample_rate.max(8_000) as usize;
        let max_frames = ((rate * MIC_MAX_LATENCY_MS) / 1_000).min(MIC_RING_CAPACITY_FRAMES - 64);
        let target_frames = (rate * MIC_TARGET_LATENCY_MS) / 1_000;
        let r = self.read_idx.load(Ordering::Relaxed);
        let w = self.write_idx.load(Ordering::Acquire);
        let available = w.wrapping_sub(r);
        if available > max_frames {
            let skip = available - target_frames;
            self.read_idx.store(r.wrapping_add(skip), Ordering::Release);
        }
    }
}

impl Default for LockFreeStereoRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Pushes native microphone input samples as exact stereo `(L, R)` frames into `ring`.
///
/// Mono inputs (`in_channels == 1`) duplicate each sample into both Left and Right channels
/// so one input sample produces exactly one stereo frame without halving playback speed.
#[inline]
pub fn push_mic_samples<T>(ring: &LockFreeStereoRingBuffer, data: &[T], in_channels: usize)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = in_channels.max(1);
    for frame in data.chunks_exact(ch) {
        let (l, r) = if ch == 1 {
            let s = f32::from_sample(frame[0]);
            (s, s)
        } else {
            (f32::from_sample(frame[0]), f32::from_sample(frame[1]))
        };
        ring.push_frame(l, r);
    }
}

/// Lock-free fractional linear resampler for physical microphone passthrough (`1.0x` pitch).
///
/// Consumes stereo frames at the physical mic's native sample rate (`mic_rate`) and produces
/// stereo frames at the virtual mic output device's sample rate (`out_rate`), strictly isolated
/// from the soundboard Pitch/Speed slider.
pub struct MicPassthroughResampler {
    prev: (f32, f32),
    cur: (f32, f32),
    frac: f64,
    primed: bool,
}

impl Default for MicPassthroughResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl MicPassthroughResampler {
    pub fn new() -> Self {
        Self {
            prev: (0.0, 0.0),
            cur: (0.0, 0.0),
            frac: 0.0,
            primed: false,
        }
    }

    pub fn reset(&mut self) {
        self.prev = (0.0, 0.0);
        self.cur = (0.0, 0.0);
        self.frac = 0.0;
        self.primed = false;
    }

    /// Prepares the resampler once per output callback buffer with initial pre-buffering.
    #[inline]
    pub fn begin_buffer(&mut self, ring: &LockFreeStereoRingBuffer, mic_rate: u32) {
        ring.trim_excess_latency(mic_rate);
        if !self.primed {
            let min_buffer = ((mic_rate.max(8_000) as usize * MIC_PREBUFFER_MS) / 1_000).max(2);
            if ring.len_frames() >= min_buffer
                && let Some(f0) = ring.pop_frame()
            {
                self.prev = f0;
                self.cur = f0;
                self.frac = 0.0;
                self.primed = true;
            }
        }
    }

    /// Produces one stereo `(L, R)` frame at `out_rate` from `ring` running at `mic_rate`.
    #[inline]
    pub fn next_stereo_frame(
        &mut self,
        ring: &LockFreeStereoRingBuffer,
        mic_rate: u32,
        out_rate: f64,
    ) -> (f32, f32) {
        if !self.primed {
            return (0.0, 0.0);
        }

        while self.frac >= 1.0 {
            if let Some(next_frame) = ring.pop_frame() {
                self.prev = self.cur;
                self.cur = next_frame;
                self.frac -= 1.0;
            } else {
                // Momentary underrun: hold/decay smoothly without dropping `primed`,
                // avoiding a repeated pre-buffer silence cycle across WASAPI packet boundaries.
                self.prev = (self.cur.0 * 0.95, self.cur.1 * 0.95);
                self.cur = self.prev;
                self.frac = 0.0;
                break;
            }
        }

        let t = self.frac as f32;
        let out_l = self.prev.0 + t * (self.cur.0 - self.prev.0);
        let out_r = self.prev.1 + t * (self.cur.1 - self.prev.1);

        let step = mic_rate.max(1) as f64 / out_rate.max(1.0);
        self.frac += step;
        (out_l, out_r)
    }
}

/// Decodes an audio file offline using `rodio::Decoder` and normalizes it to 48 kHz stereo `f32`.
pub fn decode_audio_file(path: &Path) -> anyhow::Result<DecodedSoundBuffer> {
    let file = File::open(path)
        .with_context(|| format!("failed to open audio file {}", path.display()))?;
    let mut src = rodio::Decoder::new(BufReader::new(file))
        .with_context(|| format!("failed to decode audio file {}", path.display()))?;
    let mut current_ch = src.channels().get() as usize;
    let mut current_sr = src.sample_rate().get();
    let mut segment: Vec<f32> = Vec::new();
    let mut remaining = src.current_span_len();
    let mut normalized_stereo_48k: Vec<f32> = Vec::new();

    while let Some(sample) = src.next() {
        segment.push(sample);
        if let Some(ref mut rem) = remaining {
            *rem = rem.saturating_sub(1);
            if *rem == 0 {
                let next_ch = src.channels().get() as usize;
                let next_sr = src.sample_rate().get();
                if next_ch != current_ch || next_sr != current_sr {
                    normalize_to_stereo_48k_into(
                        &segment,
                        current_ch,
                        current_sr,
                        &mut normalized_stereo_48k,
                    );
                    segment.clear();
                    current_ch = next_ch;
                    current_sr = next_sr;
                }
                remaining = src.current_span_len();
            }
        }
    }

    if !segment.is_empty() {
        normalize_to_stereo_48k_into(
            &segment,
            current_ch,
            current_sr,
            &mut normalized_stereo_48k,
        );
    }

    let frame_count = normalized_stereo_48k.len() / TARGET_CHANNELS as usize;
    let duration_secs = frame_count as f32 / TARGET_SAMPLE_RATE as f32;

    Ok(DecodedSoundBuffer {
        samples: Arc::new(normalized_stereo_48k),
        frame_count,
        duration_secs,
    })
}

/// Normalizes interleaved PCM samples from arbitrary channels/sample rate into 48 kHz stereo.
pub fn normalize_to_stereo_48k(
    raw_samples: &[f32],
    src_channels: usize,
    src_sample_rate: u32,
) -> Vec<f32> {
    let mut out = Vec::new();
    normalize_to_stereo_48k_into(raw_samples, src_channels, src_sample_rate, &mut out);
    out
}

fn normalize_to_stereo_48k_into(
    raw_samples: &[f32],
    src_channels: usize,
    src_sample_rate: u32,
    out: &mut Vec<f32>,
) {
    let channels = src_channels.max(1);
    let src_frames = raw_samples.len() / channels;
    if src_frames == 0 {
        return;
    }

    let sr = src_sample_rate.max(1);
    let mut stereo_src = Vec::with_capacity(src_frames * 2);
    for frame in raw_samples.chunks_exact(channels) {
        let (l, r) = match channels {
            1 => (frame[0], frame[0]),
            2 => (frame[0], frame[1]),
            n => {
                // Fold multi-channel layouts into balanced stereo without clipping.
                let mut left_acc = 0.0f32;
                let mut right_acc = 0.0f32;
                let mut left_n = 0usize;
                let mut right_n = 0usize;
                for (idx, &sample) in frame.iter().enumerate().take(n) {
                    if idx % 2 == 0 {
                        left_acc += sample;
                        left_n += 1;
                    } else {
                        right_acc += sample;
                        right_n += 1;
                    }
                }
                let l = if left_n > 0 {
                    left_acc / left_n as f32
                } else {
                    0.0
                };
                let r = if right_n > 0 {
                    right_acc / right_n as f32
                } else {
                    l
                };
                (l, r)
            }
        };
        stereo_src.push(l);
        stereo_src.push(r);
    }

    if sr == TARGET_SAMPLE_RATE {
        out.extend_from_slice(&stereo_src);
        return;
    }

    let ratio = sr as f64 / TARGET_SAMPLE_RATE as f64;
    let dst_frames = ((src_frames as f64 * TARGET_SAMPLE_RATE as f64) / sr as f64)
        .ceil()
        .max(1.0) as usize;
    out.reserve(dst_frames * 2);

    for i in 0..dst_frames {
        let pos = i as f64 * ratio;
        if let Some((l, r)) = sample_stereo_linear(&stereo_src, pos) {
            out.push(l);
            out.push(r);
        } else {
            break;
        }
    }
}

/// Samples a normalized stereo `f32` buffer at fractional frame `frame_pos` using linear interpolation.
#[inline]
pub fn sample_stereo_linear(samples: &[f32], frame_pos: f64) -> Option<(f32, f32)> {
    let frame_count = samples.len() / 2;
    if frame_count == 0 || frame_pos < 0.0 {
        return None;
    }

    let idx0 = frame_pos.floor() as usize;
    if idx0 >= frame_count {
        return None;
    }
    let idx1 = (idx0 + 1).min(frame_count - 1);
    let frac = (frame_pos - idx0 as f64) as f32;

    let base0 = idx0 * 2;
    let base1 = idx1 * 2;
    let l = samples[base0] + frac * (samples[base1] - samples[base0]);
    let r = samples[base0 + 1] + frac * (samples[base1 + 1] - samples[base0 + 1]);
    Some((l, r))
}

#[inline]
fn soft_clip(x: f32) -> f32 {
    if x.abs() <= 0.9 {
        x
    } else {
        let sign = x.signum();
        let excess = x.abs() - 0.9;
        sign * (0.9 + 0.1 * (excess / 0.1).tanh())
    }
}

struct SharedMixerState {
    speed_bits: AtomicU32,
    mic_volume_bits: AtomicU32,
    headphones_volume_bits: AtomicU32,
    mic_enabled: AtomicBool,
    headphones_enabled: AtomicBool,
    mic_passthrough_enabled: AtomicBool,
    mic_sample_rate: AtomicU32,
    mic_in_frames_total: AtomicU64,
    vmic_out_frames_total: AtomicU64,
    measured_mic_in_fps: AtomicU32,
    measured_vmic_out_fps: AtomicU32,
    is_playing: AtomicBool,
    progress_bits: AtomicU32,
    active_frame_count: AtomicUsize,
    playback_pos_frames_bits: AtomicU64,
    play_epoch: AtomicU64,
    seek_epoch: AtomicU64,
    seek_target_frames_bits: AtomicU64,
    leader_stream: AtomicU8,
    active_sound: Mutex<Option<ActiveSound>>,
    active_sound_id: RwLock<Option<String>>,
    mic_ring: LockFreeStereoRingBuffer,
    status: RwLock<AudioEngineStatus>,
}

impl SharedMixerState {
    fn new(config: &AudioRoutingConfig) -> Self {
        let speed = config
            .playback_speed
            .clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
        let mic_vol = config
            .mic_volume
            .clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
        let hp_vol = config
            .headphones_volume
            .clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);

        Self {
            speed_bits: AtomicU32::new(speed.to_bits()),
            mic_volume_bits: AtomicU32::new(mic_vol.to_bits()),
            headphones_volume_bits: AtomicU32::new(hp_vol.to_bits()),
            mic_enabled: AtomicBool::new(config.mic_stream_enabled),
            headphones_enabled: AtomicBool::new(config.headphones_stream_enabled),
            mic_passthrough_enabled: AtomicBool::new(config.mic_passthrough_enabled),
            mic_sample_rate: AtomicU32::new(TARGET_SAMPLE_RATE),
            mic_in_frames_total: AtomicU64::new(0),
            vmic_out_frames_total: AtomicU64::new(0),
            measured_mic_in_fps: AtomicU32::new(0),
            measured_vmic_out_fps: AtomicU32::new(0),
            is_playing: AtomicBool::new(false),
            progress_bits: AtomicU32::new(0.0f32.to_bits()),
            active_frame_count: AtomicUsize::new(0),
            playback_pos_frames_bits: AtomicU64::new(0.0f64.to_bits()),
            play_epoch: AtomicU64::new(0),
            seek_epoch: AtomicU64::new(0),
            seek_target_frames_bits: AtomicU64::new(0.0f64.to_bits()),
            leader_stream: AtomicU8::new(LEADER_FALLBACK),
            active_sound: Mutex::new(None),
            active_sound_id: RwLock::new(None),
            mic_ring: LockFreeStereoRingBuffer::new(),
            status: RwLock::new(AudioEngineStatus {
                backend_name: "Software Mixer + Loopback",
                has_warning: false,
                summary: "Initializing audio streams...".to_string(),
                warnings: Vec::new(),
                active_mic_input: None,
                active_virtual_mic_output: None,
                active_headphones_output: None,
                mic_ring_fill_ms: 0.0,
                measured_mic_in_fps: 0,
                measured_vmic_out_fps: 0,
            }),
        }
    }

    #[inline]
    fn speed(&self) -> f32 {
        f32::from_bits(self.speed_bits.load(Ordering::Relaxed))
    }

    #[inline]
    fn mic_volume(&self) -> f32 {
        f32::from_bits(self.mic_volume_bits.load(Ordering::Relaxed))
    }

    #[inline]
    fn headphones_volume(&self) -> f32 {
        f32::from_bits(self.headphones_volume_bits.load(Ordering::Relaxed))
    }
}

enum MixerCommand {
    ApplyConfig(AudioRoutingConfig),
    Shutdown,
}

/// Real-time lock-free dual-stream software mixer implementing [`AudioBackend`].
pub struct SoftwareMixerBackend {
    shared: Arc<SharedMixerState>,
    tx: Sender<MixerCommand>,
    manager_thread: Option<JoinHandle<()>>,
}

impl SoftwareMixerBackend {
    pub fn new(initial_config: &AudioRoutingConfig) -> Self {
        let shared = Arc::new(SharedMixerState::new(initial_config));
        let (tx, rx) = mpsc::channel::<MixerCommand>();
        let thread_shared = Arc::clone(&shared);
        let initial_cfg = initial_config.clone();

        let handle = thread::Builder::new()
            .name("wawefx-audio-manager".to_string())
            .spawn(move || {
                audio_manager_loop(thread_shared, rx, initial_cfg);
            })
            .expect("failed to spawn audio manager thread");

        Self {
            shared,
            tx,
            manager_thread: Some(handle),
        }
    }

    /// Reads the shared state synchronously for fallback-mode unit testing or diagnostics.
    #[cfg(test)]
    fn shared_state(&self) -> &SharedMixerState {
        &self.shared
    }
}

impl Drop for SoftwareMixerBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(MixerCommand::Shutdown);
        if let Some(handle) = self.manager_thread.take() {
            let _ = handle.join();
        }
    }
}

impl AudioBackend for SoftwareMixerBackend {
    fn backend_name(&self) -> &'static str {
        "Software Mixer + Loopback"
    }

    fn apply_routing_config(&self, config: &AudioRoutingConfig) {
        self.set_speed(config.playback_speed);
        self.set_mic_volume(config.mic_volume);
        self.set_headphones_volume(config.headphones_volume);
        self.set_mic_enabled(config.mic_stream_enabled);
        self.set_headphones_enabled(config.headphones_stream_enabled);
        self.shared
            .mic_passthrough_enabled
            .store(config.mic_passthrough_enabled, Ordering::Release);
        let _ = self.tx.send(MixerCommand::ApplyConfig(config.clone()));
    }

    fn play_sound(&self, sound: ActiveSound) {
        let sound_id = sound.id.clone();
        let frame_count = sound.frame_count;
        if let Ok(mut guard) = self.shared.active_sound.lock() {
            *guard = Some(sound);
        }
        if let Ok(mut id_guard) = self.shared.active_sound_id.write() {
            *id_guard = Some(sound_id);
        }
        self.shared
            .active_frame_count
            .store(frame_count, Ordering::Release);
        self.shared
            .seek_target_frames_bits
            .store(0.0f64.to_bits(), Ordering::Release);
        self.shared
            .playback_pos_frames_bits
            .store(0.0f64.to_bits(), Ordering::Release);
        self.shared
            .progress_bits
            .store(0.0f32.to_bits(), Ordering::Release);
        self.shared.is_playing.store(true, Ordering::Release);
        self.shared.play_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn stop(&self) {
        self.shared.is_playing.store(false, Ordering::Release);
        self.shared.active_frame_count.store(0, Ordering::Release);
        if let Ok(mut guard) = self.shared.active_sound.lock() {
            *guard = None;
        }
        if let Ok(mut id_guard) = self.shared.active_sound_id.write() {
            *id_guard = None;
        }
        self.shared
            .seek_target_frames_bits
            .store(0.0f64.to_bits(), Ordering::Release);
        self.shared
            .playback_pos_frames_bits
            .store(0.0f64.to_bits(), Ordering::Release);
        self.shared
            .progress_bits
            .store(0.0f32.to_bits(), Ordering::Release);
        self.shared.play_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn seek_relative_secs(&self, delta_secs: f32) {
        let frame_count = self.shared.active_frame_count.load(Ordering::Acquire);
        if frame_count == 0 || !self.shared.is_playing.load(Ordering::Acquire) {
            return;
        }

        let cur_frames = f64::from_bits(self.shared.playback_pos_frames_bits.load(Ordering::Acquire));
        let delta_frames = delta_secs as f64 * TARGET_SAMPLE_RATE as f64;
        let max_frame = frame_count.saturating_sub(1) as f64;
        let target = (cur_frames + delta_frames).clamp(0.0, max_frame);
        let progress = (target as f32 / frame_count as f32).clamp(0.0, 1.0);

        self.shared
            .seek_target_frames_bits
            .store(target.to_bits(), Ordering::Release);
        self.shared
            .playback_pos_frames_bits
            .store(target.to_bits(), Ordering::Release);
        self.shared
            .progress_bits
            .store(progress.to_bits(), Ordering::Release);
        self.shared.seek_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn seek_to_progress(&self, progress: f32) {
        let frame_count = self.shared.active_frame_count.load(Ordering::Acquire);
        if frame_count == 0 || !self.shared.is_playing.load(Ordering::Acquire) {
            return;
        }

        let clamped_progress = progress.clamp(0.0, 1.0);
        let max_frame = frame_count.saturating_sub(1) as f64;
        let target = (clamped_progress as f64 * max_frame).clamp(0.0, max_frame);

        self.shared
            .seek_target_frames_bits
            .store(target.to_bits(), Ordering::Release);
        self.shared
            .playback_pos_frames_bits
            .store(target.to_bits(), Ordering::Release);
        self.shared
            .progress_bits
            .store(clamped_progress.to_bits(), Ordering::Release);
        self.shared.seek_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn set_speed(&self, speed: f32) {
        let clamped = speed.clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
        self.shared
            .speed_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    fn speed(&self) -> f32 {
        self.shared.speed()
    }

    fn set_mic_volume(&self, volume: f32) {
        let clamped = volume.clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
        self.shared
            .mic_volume_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    fn mic_volume(&self) -> f32 {
        self.shared.mic_volume()
    }

    fn set_headphones_volume(&self, volume: f32) {
        let clamped = volume.clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
        self.shared
            .headphones_volume_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    fn headphones_volume(&self) -> f32 {
        self.shared.headphones_volume()
    }

    fn set_mic_enabled(&self, enabled: bool) {
        self.shared.mic_enabled.store(enabled, Ordering::Release);
    }

    fn mic_enabled(&self) -> bool {
        self.shared.mic_enabled.load(Ordering::Acquire)
    }

    fn set_headphones_enabled(&self, enabled: bool) {
        self.shared
            .headphones_enabled
            .store(enabled, Ordering::Release);
    }

    fn headphones_enabled(&self) -> bool {
        self.shared.headphones_enabled.load(Ordering::Acquire)
    }

    fn is_playing(&self) -> bool {
        self.shared.is_playing.load(Ordering::Acquire)
    }

    fn playback_progress(&self) -> f32 {
        f32::from_bits(self.shared.progress_bits.load(Ordering::Acquire))
    }

    fn active_sound_duration_secs(&self) -> f32 {
        if !self.is_playing() {
            return 0.0;
        }
        let frames = self.shared.active_frame_count.load(Ordering::Acquire);
        frames as f32 / TARGET_SAMPLE_RATE as f32
    }

    fn active_sound_id(&self) -> Option<String> {
        if !self.is_playing() {
            return None;
        }
        self.shared
            .active_sound_id
            .read()
            .ok()
            .and_then(|g| g.clone())
    }

    fn status_snapshot(&self) -> AudioEngineStatus {
        let mut status = self
            .shared
            .status
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let mic_rate = self.shared.mic_sample_rate.load(Ordering::Acquire).max(1) as f32;
        status.mic_ring_fill_ms = (self.shared.mic_ring.len_frames() as f32 / mic_rate) * 1000.0;
        status.measured_mic_in_fps = self.shared.measured_mic_in_fps.load(Ordering::Relaxed);
        status.measured_vmic_out_fps = self.shared.measured_vmic_out_fps.load(Ordering::Relaxed);
        status
    }

    fn available_devices(&self) -> AudioDeviceList {
        enumerate_cpal_devices()
    }
}

fn device_display_name(dev: &cpal::Device) -> Option<String> {
    dev.description().ok().map(|d| d.name().to_string())
}

/// Enumerates system audio input/output devices cleanly without panicking.
pub fn enumerate_cpal_devices() -> AudioDeviceList {
    let host = cpal::default_host();
    let mut input_devices = Vec::new();
    let mut output_devices = Vec::new();

    if let Ok(devs) = host.input_devices() {
        for dev in devs {
            if let Some(name) = device_display_name(&dev) {
                input_devices.push(name);
            }
        }
    }
    if let Ok(devs) = host.output_devices() {
        for dev in devs {
            if let Some(name) = device_display_name(&dev) {
                output_devices.push(name);
            }
        }
    }
    AudioDeviceList {
        input_devices,
        output_devices,
    }
}

struct ActiveStreams {
    _mic_input: Option<Stream>,
    _virtual_mic_output: Option<Stream>,
    _headphones_output: Option<Stream>,
}

fn audio_manager_loop(
    shared: Arc<SharedMixerState>,
    rx: mpsc::Receiver<MixerCommand>,
    initial_cfg: AudioRoutingConfig,
) {
    let mut current_cfg = initial_cfg;
    let mut streams = rebuild_streams(&shared, &current_cfg);
    let mut last_tick = Instant::now();
    let mut last_fps_tick = Instant::now();
    let mut prev_in_frames = shared.mic_in_frames_total.load(Ordering::Relaxed);
    let mut prev_out_frames = shared.vmic_out_frames_total.load(Ordering::Relaxed);

    loop {
        let timeout = if shared.leader_stream.load(Ordering::Acquire) == LEADER_FALLBACK
            && shared.is_playing.load(Ordering::Acquire)
        {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };

        match rx.recv_timeout(timeout) {
            Ok(MixerCommand::ApplyConfig(next_cfg)) => {
                let device_changed = current_cfg.physical_mic_device != next_cfg.physical_mic_device
                    || current_cfg.virtual_mic_output_device != next_cfg.virtual_mic_output_device
                    || current_cfg.headphones_output_device != next_cfg.headphones_output_device
                    || current_cfg.mic_passthrough_enabled != next_cfg.mic_passthrough_enabled;
                current_cfg = next_cfg;
                if device_changed {
                    // Drop old streams before opening replacements so WASAPI releases handles.
                    drop(streams);
                    streams = rebuild_streams(&shared, &current_cfg);
                    prev_in_frames = shared.mic_in_frames_total.load(Ordering::Relaxed);
                    prev_out_frames = shared.vmic_out_frames_total.load(Ordering::Relaxed);
                    last_fps_tick = Instant::now();
                }
            }
            Ok(MixerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                drop(streams);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        let dt = now.saturating_duration_since(last_tick).as_secs_f64();
        last_tick = now;

        let fps_elapsed = now.saturating_duration_since(last_fps_tick).as_secs_f64();
        if fps_elapsed >= 0.5 {
            let cur_in = shared.mic_in_frames_total.load(Ordering::Relaxed);
            let cur_out = shared.vmic_out_frames_total.load(Ordering::Relaxed);
            let in_fps = (cur_in.saturating_sub(prev_in_frames) as f64 / fps_elapsed).round() as u32;
            let out_fps =
                (cur_out.saturating_sub(prev_out_frames) as f64 / fps_elapsed).round() as u32;
            shared.measured_mic_in_fps.store(in_fps, Ordering::Relaxed);
            shared.measured_vmic_out_fps.store(out_fps, Ordering::Relaxed);
            prev_in_frames = cur_in;
            prev_out_frames = cur_out;
            last_fps_tick = now;
        }

        if shared.leader_stream.load(Ordering::Acquire) == LEADER_FALLBACK
            && shared.is_playing.load(Ordering::Acquire)
        {
            advance_fallback_clock(&shared, dt);
        }
    }
}

fn advance_fallback_clock(shared: &SharedMixerState, dt_secs: f64) {
    let frame_count = match shared.active_sound.try_lock() {
        Ok(guard) => guard.as_ref().map(|s| s.frame_count).unwrap_or(0),
        Err(_) => return,
    };
    if frame_count == 0 {
        shared.is_playing.store(false, Ordering::Release);
        return;
    }

    let speed = shared.speed() as f64;
    let cur = f64::from_bits(shared.playback_pos_frames_bits.load(Ordering::Acquire));
    let next = cur + dt_secs * TARGET_SAMPLE_RATE as f64 * speed;
    if next >= frame_count as f64 {
        shared.is_playing.store(false, Ordering::Release);
        shared
            .playback_pos_frames_bits
            .store(0.0f64.to_bits(), Ordering::Release);
        shared
            .progress_bits
            .store(0.0f32.to_bits(), Ordering::Release);
    } else {
        let progress = (next as f32 / frame_count as f32).clamp(0.0, 1.0);
        shared
            .playback_pos_frames_bits
            .store(next.to_bits(), Ordering::Release);
        shared
            .progress_bits
            .store(progress.to_bits(), Ordering::Release);
    }
}

fn find_input_device(
    host: &cpal::Host,
    requested_name: Option<&str>,
    allow_default: bool,
) -> (Option<cpal::Device>, Option<String>, Option<String>) {
    match requested_name {
        Some(name) => {
            if let Ok(mut devs) = host.input_devices()
                && let Some(dev) =
                    devs.find(|d| device_display_name(d).as_deref() == Some(name))
            {
                return (Some(dev), Some(name.to_string()), None);
            }
            (
                None,
                None,
                Some(format!("Physical Mic '{name}' was not found or is disconnected.")),
            )
        }
        None if allow_default => match host.default_input_device() {
            Some(dev) => {
                let dev_name =
                    device_display_name(&dev).unwrap_or_else(|| "Default Input".to_string());
                (Some(dev), Some(format!("{dev_name} (Default)")), None)
            }
            None => (
                None,
                None,
                Some("No default physical microphone input device detected.".to_string()),
            ),
        },
        None => (None, None, None),
    }
}

fn find_output_device(
    host: &cpal::Host,
    requested_name: Option<&str>,
    allow_default: bool,
    role_label: &str,
) -> (Option<cpal::Device>, Option<String>, Option<String>) {
    match requested_name {
        Some(name) => {
            if let Ok(mut devs) = host.output_devices()
                && let Some(dev) =
                    devs.find(|d| device_display_name(d).as_deref() == Some(name))
            {
                return (Some(dev), Some(name.to_string()), None);
            }
            (
                None,
                None,
                Some(format!("{role_label} '{name}' was not found or is disconnected.")),
            )
        }
        None if allow_default => match host.default_output_device() {
            Some(dev) => {
                let dev_name =
                    device_display_name(&dev).unwrap_or_else(|| "Default Output".to_string());
                (Some(dev), Some(format!("{dev_name} (Default)")), None)
            }
            None => (
                None,
                None,
                Some(format!("No default {role_label} device detected.")),
            ),
        },
        None => (
            None,
            None,
            Some(format!(
                "{role_label} is not selected. Open Settings (Gear icon) to choose one."
            )),
        ),
    }
}

fn rebuild_streams(shared: &Arc<SharedMixerState>, cfg: &AudioRoutingConfig) -> ActiveStreams {
    shared.mic_ring.clear();
    shared.mic_in_frames_total.store(0, Ordering::Relaxed);
    shared.vmic_out_frames_total.store(0, Ordering::Relaxed);
    shared.measured_mic_in_fps.store(0, Ordering::Relaxed);
    shared.measured_vmic_out_fps.store(0, Ordering::Relaxed);
    let host = cpal::default_host();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Headphones / Local Monitoring Output (falls back to system default output if unset).
    let (hp_dev, mut hp_label, hp_warn) = find_output_device(
        &host,
        cfg.headphones_output_device.as_deref(),
        true,
        "Headphones output",
    );
    if let Some(w) = hp_warn {
        warnings.push(w);
    }

    // 2. Virtual Mic / Voice Injection Output (must be selected explicitly so we don't hijack speakers).
    let (vmic_dev, mut vmic_label, vmic_warn) = find_output_device(
        &host,
        cfg.virtual_mic_output_device.as_deref(),
        false,
        "Virtual Mic output",
    );
    if let Some(w) = vmic_warn {
        warnings.push(w);
    }

    // 3. Physical Mic Input (opened whenever Virtual Mic output & passthrough are active).
    let need_mic_input = vmic_dev.is_some() && cfg.mic_passthrough_enabled;
    let (mic_dev, mut mic_label, mic_warn) = find_input_device(
        &host,
        cfg.physical_mic_device.as_deref(),
        need_mic_input,
    );
    if let Some(w) = mic_warn {
        warnings.push(w);
    }

    let headphones_stream = hp_dev.and_then(|dev| {
        match open_output_stream(&dev, Arc::clone(shared), OutputRole::Headphones) {
            Ok((stream, rate, ch, fmt)) => {
                if let Some(ref mut label) = hp_label {
                    *label = format!("{label} ({rate} Hz, {ch} ch, {fmt})");
                }
                Some(stream)
            }
            Err(err) => {
                warnings.push(format!("Failed to open Headphones output: {err}"));
                hp_label = None;
                None
            }
        }
    });

    // Open Physical Mic input BEFORE Virtual Mic output so `shared.mic_sample_rate` is set first.
    let mic_input_stream = if need_mic_input {
        mic_dev.and_then(|dev| match open_mic_input_stream(&dev, Arc::clone(shared)) {
            Ok((stream, rate, ch, fmt)) => {
                if let Some(ref mut label) = mic_label {
                    *label = format!("{label} ({rate} Hz, {ch} ch, {fmt})");
                }
                Some(stream)
            }
            Err(err) => {
                warnings.push(format!("Failed to open Physical Mic input: {err}"));
                mic_label = None;
                None
            }
        })
    } else {
        mic_label = None;
        None
    };

    let virtual_mic_stream = vmic_dev.and_then(|dev| {
        match open_output_stream(&dev, Arc::clone(shared), OutputRole::VirtualMic) {
            Ok((stream, rate, ch, fmt)) => {
                if let Some(ref mut label) = vmic_label {
                    *label = format!("{label} ({rate} Hz, {ch} ch, {fmt})");
                }
                Some(stream)
            }
            Err(err) => {
                warnings.push(format!("Failed to open Virtual Mic output: {err}"));
                vmic_label = None;
                None
            }
        }
    });

    let leader = if headphones_stream.is_some() {
        LEADER_HEADPHONES
    } else if virtual_mic_stream.is_some() {
        LEADER_VIRTUAL_MIC
    } else {
        LEADER_FALLBACK
    };
    shared.leader_stream.store(leader, Ordering::Release);

    let has_warning = !warnings.is_empty();
    let summary = if has_warning {
        warnings[0].clone()
    } else {
        "Audio engine active (dual routing ready).".to_string()
    };

    if let Ok(mut status_guard) = shared.status.write() {
        *status_guard = AudioEngineStatus {
            backend_name: "Software Mixer + Loopback",
            has_warning,
            summary,
            warnings,
            active_mic_input: mic_label,
            active_virtual_mic_output: vmic_label,
            active_headphones_output: hp_label,
            mic_ring_fill_ms: 0.0,
            measured_mic_in_fps: 0,
            measured_vmic_out_fps: 0,
        };
    }

    ActiveStreams {
        _mic_input: mic_input_stream,
        _virtual_mic_output: virtual_mic_stream,
        _headphones_output: headphones_stream,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputRole {
    Headphones,
    VirtualMic,
}

impl OutputRole {
    #[inline]
    fn leader_id(self) -> u8 {
        match self {
            OutputRole::Headphones => LEADER_HEADPHONES,
            OutputRole::VirtualMic => LEADER_VIRTUAL_MIC,
        }
    }
}

fn open_mic_input_stream(
    device: &cpal::Device,
    shared: Arc<SharedMixerState>,
) -> anyhow::Result<(Stream, u32, u16, SampleFormat)> {
    let supported = device
        .default_input_config()
        .context("failed to query default input format")?;
    let sample_format = supported.sample_format();
    let stream_cfg: StreamConfig = supported.into();
    let rate = stream_cfg.sample_rate.max(1);
    let ch = stream_cfg.channels.max(1);
    shared.mic_sample_rate.store(rate, Ordering::Release);

    let stream = match sample_format {
        SampleFormat::F32 => build_mic_input_typed::<f32>(device, &stream_cfg, shared)?,
        SampleFormat::I16 => build_mic_input_typed::<i16>(device, &stream_cfg, shared)?,
        SampleFormat::U16 => build_mic_input_typed::<u16>(device, &stream_cfg, shared)?,
        SampleFormat::I24 => build_mic_input_typed::<cpal::I24>(device, &stream_cfg, shared)?,
        SampleFormat::U24 => build_mic_input_typed::<cpal::U24>(device, &stream_cfg, shared)?,
        SampleFormat::I32 => build_mic_input_typed::<i32>(device, &stream_cfg, shared)?,
        SampleFormat::U32 => build_mic_input_typed::<u32>(device, &stream_cfg, shared)?,
        SampleFormat::F64 => build_mic_input_typed::<f64>(device, &stream_cfg, shared)?,
        SampleFormat::I8 => build_mic_input_typed::<i8>(device, &stream_cfg, shared)?,
        SampleFormat::U8 => build_mic_input_typed::<u8>(device, &stream_cfg, shared)?,
        other => anyhow::bail!("unsupported input sample format: {other:?}"),
    };
    stream.play().context("failed to start mic input stream")?;
    Ok((stream, rate, ch, sample_format))
}

fn build_mic_input_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<SharedMixerState>,
) -> anyhow::Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let in_channels = config.channels.max(1) as usize;

    let stream = device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                let frame_count = data.len() / in_channels;
                shared
                    .mic_in_frames_total
                    .fetch_add(frame_count as u64, Ordering::Relaxed);
                if !shared.mic_passthrough_enabled.load(Ordering::Relaxed) {
                    return;
                }
                push_mic_samples(&shared.mic_ring, data, in_channels);
            },
            |err| {
                tracing::warn!("Mic input stream error: {err}");
            },
            None,
        )
        .context("failed to build mic input stream")?;

    Ok(stream)
}

fn open_output_stream(
    device: &cpal::Device,
    shared: Arc<SharedMixerState>,
    role: OutputRole,
) -> anyhow::Result<(Stream, u32, u16, SampleFormat)> {
    let supported = device
        .default_output_config()
        .context("failed to query default output format")?;
    let sample_format = supported.sample_format();
    let stream_cfg: StreamConfig = supported.into();
    let rate = stream_cfg.sample_rate.max(1);
    let ch = stream_cfg.channels.max(1);

    let stream = match sample_format {
        SampleFormat::F32 => build_output_typed::<f32>(device, &stream_cfg, shared, role)?,
        SampleFormat::I16 => build_output_typed::<i16>(device, &stream_cfg, shared, role)?,
        SampleFormat::U16 => build_output_typed::<u16>(device, &stream_cfg, shared, role)?,
        SampleFormat::I24 => build_output_typed::<cpal::I24>(device, &stream_cfg, shared, role)?,
        SampleFormat::U24 => build_output_typed::<cpal::U24>(device, &stream_cfg, shared, role)?,
        SampleFormat::I32 => build_output_typed::<i32>(device, &stream_cfg, shared, role)?,
        SampleFormat::U32 => build_output_typed::<u32>(device, &stream_cfg, shared, role)?,
        SampleFormat::F64 => build_output_typed::<f64>(device, &stream_cfg, shared, role)?,
        SampleFormat::I8 => build_output_typed::<i8>(device, &stream_cfg, shared, role)?,
        SampleFormat::U8 => build_output_typed::<u8>(device, &stream_cfg, shared, role)?,
        other => anyhow::bail!("unsupported output sample format: {other:?}"),
    };
    stream.play().context("failed to start output stream")?;
    Ok((stream, rate, ch, sample_format))
}

fn build_output_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<SharedMixerState>,
    role: OutputRole,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let out_channels = config.channels.max(1) as usize;
    let out_rate = config.sample_rate.max(1) as f64;
    let base_rate_step = TARGET_SAMPLE_RATE as f64 / out_rate;
    let role_leader_id = role.leader_id();

    let mut local_play_epoch = 0u64;
    let mut local_seek_epoch = 0u64;
    let mut local_sound: Option<ActiveSound> = None;
    let mut local_pos_frames = 0.0f64;

    let mut mic_resampler = MicPassthroughResampler::new();

    let stream = device
        .build_output_stream(
            *config,
            move |output: &mut [T], _| {
                // 1. Check for a newly triggered sound or stop via atomic play_epoch + try_lock().
                let pe = shared.play_epoch.load(Ordering::Acquire);
                if pe != local_play_epoch
                    && let Ok(guard) = shared.active_sound.try_lock()
                {
                    local_sound = guard.clone();
                    local_pos_frames =
                        f64::from_bits(shared.seek_target_frames_bits.load(Ordering::Acquire));
                    local_play_epoch = pe;
                    local_seek_epoch = shared.seek_epoch.load(Ordering::Acquire);
                }

                // 2. Check for seek updates (`Rewind -5s` / `FastForward +5s` / Timeline seek).
                let se = shared.seek_epoch.load(Ordering::Acquire);
                if se != local_seek_epoch {
                    local_pos_frames =
                        f64::from_bits(shared.seek_target_frames_bits.load(Ordering::Acquire));
                    local_seek_epoch = se;
                }

                let playing_flag = shared.is_playing.load(Ordering::Acquire);
                let speed = shared.speed().clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED) as f64;
                let frame_step = speed * base_rate_step;
                let is_leader = shared.leader_stream.load(Ordering::Acquire) == role_leader_id;

                let (stream_enabled, stream_vol) = match role {
                    OutputRole::Headphones => (
                        shared.headphones_enabled.load(Ordering::Relaxed),
                        shared.headphones_volume(),
                    ),
                    OutputRole::VirtualMic => (
                        shared.mic_enabled.load(Ordering::Relaxed),
                        shared.mic_volume(),
                    ),
                };

                let passthrough_active = role == OutputRole::VirtualMic
                    && shared.mic_passthrough_enabled.load(Ordering::Relaxed);
                if role == OutputRole::VirtualMic {
                    let out_frame_count = output.len() / out_channels;
                    shared
                        .vmic_out_frames_total
                        .fetch_add(out_frame_count as u64, Ordering::Relaxed);
                }
                let mic_rate = shared.mic_sample_rate.load(Ordering::Acquire).max(1);
                if passthrough_active {
                    mic_resampler.begin_buffer(&shared.mic_ring, mic_rate);
                } else if role == OutputRole::VirtualMic {
                    mic_resampler.reset();
                    shared.mic_ring.clear();
                }

                for out_frame in output.chunks_exact_mut(out_channels) {
                    let mut sound_l = 0.0f32;
                    let mut sound_r = 0.0f32;

                    if playing_flag && let Some(ref snd) = local_sound {
                        if let Some((l, r)) = sample_stereo_linear(&snd.samples, local_pos_frames) {
                            let gain = if stream_enabled {
                                stream_vol * snd.volume_trim
                            } else {
                                0.0
                            };
                            sound_l = l * gain;
                            sound_r = r * gain;
                            local_pos_frames += frame_step;
                        } else {
                            local_sound = None;
                            if is_leader {
                                shared.is_playing.store(false, Ordering::Release);
                                shared
                                    .playback_pos_frames_bits
                                    .store(0.0f64.to_bits(), Ordering::Release);
                                shared
                                    .progress_bits
                                    .store(0.0f32.to_bits(), Ordering::Release);
                            }
                        }
                    }

                    // Physical mic passthrough runs strictly at 1.0x (`mic_rate / out_rate`),
                    // completely isolated from soundboard `speed`.
                    let (mic_l, mic_r) = if passthrough_active {
                        mic_resampler.next_stereo_frame(&shared.mic_ring, mic_rate, out_rate)
                    } else {
                        (0.0, 0.0)
                    };

                    let mix_l = soft_clip(mic_l + sound_l);
                    let mix_r = soft_clip(mic_r + sound_r);

                    if out_channels == 1 {
                        let mono = 0.5 * (mix_l + mix_r);
                        out_frame[0] = T::from_sample(mono);
                    } else {
                        out_frame[0] = T::from_sample(mix_l);
                        out_frame[1] = T::from_sample(mix_r);
                        for ch in out_frame.iter_mut().skip(2) {
                            *ch = T::from_sample(0.0f32);
                        }
                    }
                }

                if is_leader && shared.is_playing.load(Ordering::Acquire)
                    && let Some(ref snd) = local_sound
                {
                    let total = snd.frame_count.max(1) as f32;
                    let progress = (local_pos_frames as f32 / total).clamp(0.0, 1.0);
                    shared
                        .playback_pos_frames_bits
                        .store(local_pos_frames.to_bits(), Ordering::Release);
                    shared
                        .progress_bits
                        .store(progress.to_bits(), Ordering::Release);
                }
            },
            |err| {
                tracing::warn!("Audio output stream error: {err}");
            },
            None,
        )
        .context("failed to build audio output stream")?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ActiveSoundData;

    #[test]
    fn lock_free_ring_buffer_wraps_and_preserves_order() {
        let ring = LockFreeStereoRingBuffer::new();
        assert_eq!(ring.pop_frame(), None);

        for i in 0..200 {
            let v = i as f32 * 0.001;
            assert!(ring.push_frame(v, -v));
        }
        assert_eq!(ring.len_frames(), 200);

        for i in 0..200 {
            let v = i as f32 * 0.001;
            let (l, r) = ring.pop_frame().expect("frame available");
            assert!((l - v).abs() < 1e-6);
            assert!((r + v).abs() < 1e-6);
        }
        assert_eq!(ring.pop_frame(), None);

        // Fill to exact capacity and verify non-blocking rejection when full.
        for i in 0..MIC_RING_CAPACITY_FRAMES {
            assert!(ring.push_frame(i as f32, 0.0));
        }
        assert!(!ring.push_frame(999.0, 999.0));
    }

    #[test]
    fn normalizes_mono_24khz_to_stereo_48khz() {
        let mono_24k = vec![0.0f32, 0.5, 1.0, 0.5];
        let stereo_48k = normalize_to_stereo_48k(&mono_24k, 1, 24_000);
        // 4 frames at 24 kHz -> 8 frames at 48 kHz -> 16 stereo interleaved samples.
        assert_eq!(stereo_48k.len(), 16);
        assert!((stereo_48k[0] - 0.0).abs() < 1e-6);
        assert!((stereo_48k[1] - 0.0).abs() < 1e-6);
        // Frame 1 (pos = 0.5) interpolates between 0.0 and 0.5 -> 0.25.
        assert!((stereo_48k[2] - 0.25).abs() < 1e-6);
        assert!((stereo_48k[3] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn linear_interpolation_supports_fractional_speed_indices() {
        let stereo = vec![0.0, 1.0, 1.0, 0.0];
        let mid = sample_stereo_linear(&stereo, 0.25).expect("sample in bounds");
        assert!((mid.0 - 0.25).abs() < 1e-6);
        assert!((mid.1 - 0.75).abs() < 1e-6);
        assert!(sample_stereo_linear(&stereo, 2.0).is_none());
    }

    #[test]
    fn backend_transport_seek_and_progress_are_atomic_and_clamped() {
        let cfg = AudioRoutingConfig::default();
        let backend = SoftwareMixerBackend::new(&cfg);

        // Create a 10-second 48 kHz silent buffer.
        let frames = TARGET_SAMPLE_RATE as usize * 10;
        let sound = Arc::new(ActiveSoundData {
            id: "test-1".to_string(),
            title: "Test Tone".to_string(),
            samples: Arc::new(vec![0.1f32; frames * 2]),
            frame_count: frames,
            volume_trim: 1.0,
        });

        backend.play_sound(sound);
        assert!(backend.is_playing());
        assert_eq!(backend.active_sound_id().as_deref(), Some("test-1"));
        assert!((backend.active_sound_duration_secs() - 10.0).abs() < 0.01);

        backend.seek_to_progress(0.75);
        let prog_abs = backend.playback_progress();
        assert!((prog_abs - 0.75).abs() < 0.02, "expected ~0.75 progress, got {prog_abs}");

        backend.seek_relative_secs(-2.5);
        let prog = backend.playback_progress();
        assert!((prog - 0.5).abs() < 0.02, "expected ~0.5 progress, got {prog}");

        backend.seek_relative_secs(-10.0);
        assert!(backend.playback_progress() <= 0.02);

        backend.set_speed(3.0);
        assert_eq!(backend.speed(), MAX_PLAYBACK_SPEED);
        backend.set_speed(0.1);
        assert_eq!(backend.speed(), MIN_PLAYBACK_SPEED);

        backend.set_mic_enabled(false);
        assert!(!backend.mic_enabled());
        backend.set_headphones_enabled(true);
        assert!(backend.headphones_enabled());

        backend.stop();
        assert!(!backend.is_playing());
        assert!(backend.active_sound_id().is_none());
        assert_eq!(backend.shared_state().playback_progress_check(), 0.0);
    }

    fn measure_sine_hz(samples: &[f32], sample_rate: f64) -> f64 {
        let mut rising_crossings = Vec::new();
        for i in 1..samples.len() {
            if samples[i - 1] <= 0.0 && samples[i] > 0.0 {
                let frac = (-samples[i - 1] / (samples[i] - samples[i - 1])) as f64;
                rising_crossings.push((i - 1) as f64 + frac);
            }
        }
        assert!(rising_crossings.len() >= 5, "not enough cycles measured");
        let first = rising_crossings[1];
        let last = *rising_crossings.last().unwrap();
        let cycles = (rising_crossings.len() - 2) as f64;
        let avg_period_samples = (last - first) / cycles;
        sample_rate / avg_period_samples
    }

    #[test]
    fn mic_passthrough_preserves_pitch_across_mono_stereo_and_sample_rates() {
        for &(in_rate, in_channels) in &[(16_000u32, 1usize), (44_100u32, 1usize), (48_000u32, 2usize)]
        {
            let ring = LockFreeStereoRingBuffer::new();
            let mut resampler = MicPassthroughResampler::new();
            let out_rate = 48_000u32;
            let tone_hz = 440.0f64;

            // Generate 120 ms of 440 Hz sine input in small 10 ms chunks to mimic real callbacks.
            let chunk_in_frames = (in_rate as usize) / 100;
            let total_chunks = 12usize;
            let mut sample_cursor = 0usize;
            let mut out_left = Vec::new();
            let mut out_right = Vec::new();
            let chunk_out_frames = (out_rate as usize) / 100;

            for _ in 0..total_chunks {
                let mut input = Vec::with_capacity(chunk_in_frames * in_channels);
                for _ in 0..chunk_in_frames {
                    let s = (2.0 * std::f64::consts::PI * tone_hz * sample_cursor as f64
                        / in_rate as f64)
                        .sin() as f32;
                    sample_cursor += 1;
                    for _ in 0..in_channels {
                        input.push(s);
                    }
                }
                push_mic_samples(&ring, &input, in_channels);

                resampler.begin_buffer(&ring, in_rate);
                for _ in 0..chunk_out_frames {
                    let (l, r) = resampler.next_stereo_frame(&ring, in_rate, out_rate as f64);
                    out_left.push(l);
                    out_right.push(r);
                }
            }

            // Skip initial 35 ms pre-buffer silence before measuring pitch.
            let skip = (out_rate as usize * 35) / 1000;
            let hz_l = measure_sine_hz(&out_left[skip..], out_rate as f64);
            let hz_r = measure_sine_hz(&out_right[skip..], out_rate as f64);

            assert!(
                (hz_l - tone_hz).abs() < 4.0,
                "rate={in_rate} ch={in_channels}: expected ~{tone_hz} Hz on L, got {hz_l:.2} Hz"
            );
            assert!(
                (hz_r - tone_hz).abs() < 4.0,
                "rate={in_rate} ch={in_channels}: expected ~{tone_hz} Hz on R, got {hz_r:.2} Hz"
            );
        }
    }

    #[test]
    fn wasapi_packet_boundaries_never_trigger_silence_chopper_or_pitch_drop() {
        let ring = LockFreeStereoRingBuffer::new();
        let mut resampler = MicPassthroughResampler::new();
        let rate = 48_000u32;
        let tone_hz = 440.0f64;
        let packet_frames = 480usize; // 10 ms WASAPI packet
        let mut cursor = 0usize;
        let mut out_left = Vec::new();

        // Simulate 200 ms: feed 3 packets (30 ms), then alternate 20 ms drain & 10 ms capture jitter.
        for step in 0..20 {
            let mut input = Vec::with_capacity(packet_frames);
            for _ in 0..packet_frames {
                let s = (2.0 * std::f64::consts::PI * tone_hz * cursor as f64 / rate as f64).sin()
                    as f32;
                cursor += 1;
                input.push(s);
            }
            push_mic_samples(&ring, &input, 1);

            if step >= 2 {
                // Force a momentary underrun on step 4 by draining an extra 480 frames, then resume 480/step.
                let drain = if step == 4 { 1440 } else { 480 };
                resampler.begin_buffer(&ring, rate);
                for _ in 0..drain {
                    let (l, _) = resampler.next_stereo_frame(&ring, rate, rate as f64);
                    out_left.push(l);
                }
            }
        }

        let tail = &out_left[out_left.len() - 2400..];
        let measured = measure_sine_hz(tail, rate as f64);
        assert!(
            (measured - tone_hz).abs() < 4.0,
            "expected continuous ~{tone_hz} Hz after packet boundary underrun, got {measured:.2} Hz"
        );
    }
}

#[cfg(test)]
impl SharedMixerState {
    fn playback_progress_check(&self) -> f32 {
        f32::from_bits(self.progress_bits.load(Ordering::Acquire))
    }
}
