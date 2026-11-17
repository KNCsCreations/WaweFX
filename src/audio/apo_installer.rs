//! Platform driver-level audio hook diagnostics and Windows APO stub.
//!
//! By default, WaweFX uses [`crate::audio::mixer::SoftwareMixerBackend`] (real-time `cpal`
//! loopback + software mixing) so users can route audio immediately via virtual audio cables
//! or dedicated voice output devices without installing kernel/COM drivers.
//!
//! On Windows (`#[cfg(target_os = "windows")]`), this module prepares the [`AudioBackend`]
//! interface and Windows registry metadata needed if we later compile a dedicated C++/Rust
//! COM Audio Processing Object (`WaweFXApo.dll`) and register it on an endpoint's `FxProperties`
//! key under:
//! `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture\{Endpoint-GUID}\FxProperties`

/// Status of platform driver-level audio hook registration.
#[derive(Debug, Clone)]
pub struct ApoHookStatus {
    pub dll_present: bool,
    pub registered_endpoints: Vec<String>,
    pub notes: String,
}

#[cfg(target_os = "windows")]
mod windows_apo {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::ApoHookStatus;
    use crate::audio::{ActiveSound, AudioBackend, AudioDeviceList, AudioEngineStatus};
    use crate::config::{
        AudioRoutingConfig, MAX_PLAYBACK_SPEED, MAX_STREAM_VOLUME, MIN_PLAYBACK_SPEED,
        MIN_STREAM_VOLUME,
    };

    /// Registry hive path where Windows stores per-endpoint capture APO hooks.
    pub const MMDEVICES_CAPTURE_KEY_PATH: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture";

    /// Subkey holding APO CLSIDs (`PKEY_FX_StreamEffectClsid`, `PKEY_FX_ModeEffectClsid`).
    pub const FX_PROPERTIES_SUBKEY: &str = "FxProperties";

    /// Placeholder COM CLSID reserved for a future WaweFX Capture APO DLL.
    pub const WAWEFX_APO_CLSID: &str = "{8A4E9D20-4F5B-4B9E-9A77-576177654658}";

    /// Inspects whether a compiled WaweFX APO COM DLL is available for driver-level capture injection.
    pub fn query_apo_status() -> ApoHookStatus {
        ApoHookStatus {
            dll_present: false,
            registered_endpoints: Vec::new(),
            notes: format!(
                "Driver-level APO hook stub ready (target registry: HKLM\\{MMDEVICES_CAPTURE_KEY_PATH}\\<Endpoint-GUID>\\{FX_PROPERTIES_SUBKEY}, CLSID {WAWEFX_APO_CLSID}). Using Software Mixer backend."
            ),
        }
    }

    /// Stub for registering the future WaweFX APO CLSID under a specific Windows capture endpoint GUID.
    pub fn install_apo_registry_hook_stub(endpoint_guid: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "APO driver DLL ({WAWEFX_APO_CLSID}) is not bundled yet; cannot register HKLM\\{MMDEVICES_CAPTURE_KEY_PATH}\\{endpoint_guid}\\{FX_PROPERTIES_SUBKEY}."
        )
    }

    /// Stub for removing WaweFX APO CLSID entries from a capture endpoint's `FxProperties`.
    pub fn uninstall_apo_registry_hook_stub(endpoint_guid: &str) -> anyhow::Result<()> {
        tracing::info!(
            "APO uninstall stub called for HKLM\\{MMDEVICES_CAPTURE_KEY_PATH}\\{endpoint_guid}\\{FX_PROPERTIES_SUBKEY}"
        );
        Ok(())
    }

    /// Stub [`AudioBackend`] implementation representing future driver-level APO IPC control.
    pub struct ApoHookBackend {
        speed_bits: AtomicU32,
        mic_volume_bits: AtomicU32,
        headphones_volume_bits: AtomicU32,
        mic_enabled: AtomicBool,
        headphones_enabled: AtomicBool,
    }

    impl Default for ApoHookBackend {
        fn default() -> Self {
            Self {
                speed_bits: AtomicU32::new(1.0f32.to_bits()),
                mic_volume_bits: AtomicU32::new(1.0f32.to_bits()),
                headphones_volume_bits: AtomicU32::new(1.0f32.to_bits()),
                mic_enabled: AtomicBool::new(true),
                headphones_enabled: AtomicBool::new(true),
            }
        }
    }

    impl AudioBackend for ApoHookBackend {
        fn backend_name(&self) -> &'static str {
            "Windows APO Hook (Stub)"
        }

        fn apply_routing_config(&self, config: &AudioRoutingConfig) {
            self.set_speed(config.playback_speed);
            self.set_mic_volume(config.mic_volume);
            self.set_headphones_volume(config.headphones_volume);
            self.set_mic_enabled(config.mic_stream_enabled);
            self.set_headphones_enabled(config.headphones_stream_enabled);
        }

        fn play_sound(&self, sound: ActiveSound) {
            tracing::warn!(
                "ApoHookBackend stub ignored play_sound({}) — switch to SoftwareMixerBackend.",
                sound.title
            );
        }

        fn stop(&self) {}

        fn seek_relative_secs(&self, _delta_secs: f32) {}

        fn seek_to_progress(&self, _progress: f32) {}

        fn set_speed(&self, speed: f32) {
            let clamped = speed.clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
            self.speed_bits.store(clamped.to_bits(), Ordering::Relaxed);
        }

        fn speed(&self) -> f32 {
            f32::from_bits(self.speed_bits.load(Ordering::Relaxed))
        }

        fn set_mic_volume(&self, volume: f32) {
            let clamped = volume.clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
            self.mic_volume_bits
                .store(clamped.to_bits(), Ordering::Relaxed);
        }

        fn mic_volume(&self) -> f32 {
            f32::from_bits(self.mic_volume_bits.load(Ordering::Relaxed))
        }

        fn set_headphones_volume(&self, volume: f32) {
            let clamped = volume.clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
            self.headphones_volume_bits
                .store(clamped.to_bits(), Ordering::Relaxed);
        }

        fn headphones_volume(&self) -> f32 {
            f32::from_bits(self.headphones_volume_bits.load(Ordering::Relaxed))
        }

        fn set_mic_enabled(&self, enabled: bool) {
            self.mic_enabled.store(enabled, Ordering::Relaxed);
        }

        fn mic_enabled(&self) -> bool {
            self.mic_enabled.load(Ordering::Relaxed)
        }

        fn set_headphones_enabled(&self, enabled: bool) {
            self.headphones_enabled.store(enabled, Ordering::Relaxed);
        }

        fn headphones_enabled(&self) -> bool {
            self.headphones_enabled.load(Ordering::Relaxed)
        }

        fn is_playing(&self) -> bool {
            false
        }

        fn playback_progress(&self) -> f32 {
            0.0
        }

        fn active_sound_duration_secs(&self) -> f32 {
            0.0
        }

        fn active_sound_id(&self) -> Option<String> {
            None
        }

        fn status_snapshot(&self) -> AudioEngineStatus {
            let apo = query_apo_status();
            AudioEngineStatus {
                backend_name: self.backend_name(),
                has_warning: true,
                summary: apo.notes,
                warnings: vec!["Windows APO COM DLL is not compiled/installed yet.".to_string()],
                ..Default::default()
            }
        }

        fn available_devices(&self) -> AudioDeviceList {
            crate::audio::mixer::enumerate_cpal_devices()
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_apo::{
    ApoHookBackend, FX_PROPERTIES_SUBKEY, MMDEVICES_CAPTURE_KEY_PATH, WAWEFX_APO_CLSID,
    install_apo_registry_hook_stub, query_apo_status, uninstall_apo_registry_hook_stub,
};

/// Cross-platform macOS CoreAudio status fallback (Windows APO hooks do not apply on macOS).
#[cfg(target_os = "macos")]
pub fn query_apo_status() -> ApoHookStatus {
    ApoHookStatus {
        dll_present: false,
        registered_endpoints: Vec::new(),
        notes: "CoreAudio Software Mixer active. Route Virtual Mic Output through BlackHole, Loopback, or Soundflower.".to_string(),
    }
}

/// Cross-platform fallback for non-Windows, non-macOS targets.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn query_apo_status() -> ApoHookStatus {
    ApoHookStatus {
        dll_present: false,
        registered_endpoints: Vec::new(),
        notes: "Using cross-platform Software Mixer backend.".to_string(),
    }
}
