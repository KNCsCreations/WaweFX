use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use eframe::egui::Color32;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Persistent JSON configuration file name stored in the platform config directory.
pub const CONFIG_FILE_NAME: &str = "wawefx_config.json";

/// Subdirectory inside the OS configuration directory used by WaweFX.
pub const CONFIG_APP_DIR_NAME: &str = "WaweFX";

/// Managed subdirectory inside the WaweFX AppData root holding copied sound files.
pub const SOUNDS_SUBDIR: &str = "sounds";

/// Managed subdirectory inside the WaweFX AppData root holding copied pad cover images.
pub const COVERS_SUBDIR: &str = "covers";

/// Managed subdirectory inside the WaweFX AppData root holding helper binaries (e.g. `yt-dlp`).
pub const BIN_SUBDIR: &str = "bin";

/// Uniform internal audio mixer sample rate (Hz) used across normalized sound buffers.
pub const TARGET_SAMPLE_RATE: u32 = 48_000;

/// Uniform internal audio mixer channel count (stereo).
pub const TARGET_CHANNELS: u16 = 2;

/// Minimum and maximum supported playback speed/pitch factor.
pub const MIN_PLAYBACK_SPEED: f32 = 0.5;
pub const MAX_PLAYBACK_SPEED: f32 = 2.0;

/// Minimum, maximum, and default playback pitch hotkey step size.
pub const MIN_PITCH_STEP: f32 = 0.01;
pub const MAX_PITCH_STEP: f32 = 0.50;
pub const DEFAULT_PITCH_STEP: f32 = 0.05;

/// Steps playback speed by `delta`, clamped to `[MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED]`
/// and rounded to 4 decimal places to avoid floating-point drift across repeated presses.
pub fn step_playback_speed(current: f32, delta: f32) -> f32 {
    let next = (current + delta).clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
    (next * 10_000.0).round() / 10_000.0
}

/// Minimum and maximum supported stream volume gain (0% to 200%).
pub const MIN_STREAM_VOLUME: f32 = 0.0;
pub const MAX_STREAM_VOLUME: f32 = 2.0;

/// Minimum normalized gap enforced between `trim_start` and `trim_end`.
pub const MIN_TRIM_GAP: f32 = 0.02;

fn default_one() -> f32 {
    1.0
}

/// Clamps normalized trim bounds to `[0.0, 1.0]` while preserving at least [`MIN_TRIM_GAP`].
pub fn sanitize_trim_range(start: f32, end: f32) -> (f32, f32) {
    let mut s = if start.is_finite() {
        start.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut e = if end.is_finite() {
        end.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if e - s < MIN_TRIM_GAP {
        if s + MIN_TRIM_GAP <= 1.0 {
            e = (s + MIN_TRIM_GAP).min(1.0);
        } else {
            e = 1.0;
            s = (1.0 - MIN_TRIM_GAP).max(0.0);
        }
    }
    (s, e)
}

/// Represents a single sound pad entry on the WaweFX soundboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SoundItem {
    pub id: String,
    pub title: String,
    pub audio_path: PathBuf,
    pub image_path: Option<PathBuf>,
    pub duration_secs: f32,
    pub volume_trim: f32,
    #[serde(default)]
    pub trim_start: f32,
    #[serde(default = "default_one")]
    pub trim_end: f32,
    pub shortcut: Option<String>,
}

impl Default for SoundItem {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            title: "Untitled Sound".to_string(),
            audio_path: PathBuf::new(),
            image_path: None,
            duration_secs: 0.0,
            volume_trim: 1.0,
            trim_start: 0.0,
            trim_end: 1.0,
            shortcut: None,
        }
    }
}

impl SoundItem {
    /// Creates a new [`SoundItem`] with a freshly generated UUID v4 identifier.
    pub fn new(
        title: impl Into<String>,
        audio_path: PathBuf,
        image_path: Option<PathBuf>,
        duration_secs: f32,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.into(),
            audio_path,
            image_path,
            duration_secs: duration_secs.max(0.0),
            volume_trim: 1.0,
            trim_start: 0.0,
            trim_end: 1.0,
            shortcut: None,
        }
    }

    /// Returns the active trimmed duration (seconds) after applying `trim_start..trim_end`.
    pub fn trimmed_duration_secs(&self) -> f32 {
        let (start, end) = sanitize_trim_range(self.trim_start, self.trim_end);
        (self.duration_secs.max(0.0) * (end - start)).max(0.0)
    }

    /// Formats the trimmed sound duration as `MM:SS` for display on pad cards and mobile clients.
    pub fn formatted_duration(&self) -> String {
        let total_secs = self.trimmed_duration_secs().round() as u32;
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{minutes:02}:{seconds:02}")
    }

    /// Resolves `audio_path` against the managed WaweFX AppData directory when stored relatively.
    pub fn resolved_audio_path(&self) -> PathBuf {
        AppConfig::resolve_asset_path(&self.audio_path)
    }

    /// Resolves `image_path` against the managed WaweFX AppData directory when stored relatively.
    pub fn resolved_image_path(&self) -> Option<PathBuf> {
        self.image_path
            .as_deref()
            .map(AppConfig::resolve_asset_path)
    }
}

/// Persistent audio device selection, routing toggles, and gain/pitch states.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioRoutingConfig {
    pub physical_mic_device: Option<String>,
    pub virtual_mic_output_device: Option<String>,
    pub headphones_output_device: Option<String>,
    pub mic_passthrough_enabled: bool,
    pub mic_stream_enabled: bool,
    pub mic_volume: f32,
    pub headphones_stream_enabled: bool,
    pub headphones_volume: f32,
    pub playback_speed: f32,
    pub pitch_step: f32,
    pub sync_port: u16,
}

impl Default for AudioRoutingConfig {
    fn default() -> Self {
        Self {
            physical_mic_device: None,
            virtual_mic_output_device: None,
            headphones_output_device: None,
            mic_passthrough_enabled: true,
            mic_stream_enabled: true,
            mic_volume: 1.0,
            headphones_stream_enabled: true,
            headphones_volume: 1.0,
            playback_speed: 1.0,
            pitch_step: DEFAULT_PITCH_STEP,
            sync_port: 8765,
        }
    }
}

/// Available color theme presets in Settings, plus `Custom` when RGB values or a user preset are active.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ThemePreset {
    WarmBeige,
    #[default]
    MidnightDark,
    NordFrost,
    Custom,
}

impl ThemePreset {
    /// The 3 built-in presets displayed in the Settings theme selector.
    pub const BUILTIN_PRESETS: [ThemePreset; 3] = [
        ThemePreset::MidnightDark,
        ThemePreset::WarmBeige,
        ThemePreset::NordFrost,
    ];

    /// Human-readable display label for the Settings UI.
    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::WarmBeige => "Warm Beige",
            ThemePreset::MidnightDark => "Midnight Dark",
            ThemePreset::NordFrost => "Nord Frost",
            ThemePreset::Custom => "Custom",
        }
    }
}

/// User-saved named color palette stored in `wawefx_config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NamedTheme {
    pub name: String,
    pub theme: ThemeConfig,
}

impl Default for NamedTheme {
    fn default() -> Self {
        Self {
            name: "Custom Preset".to_string(),
            theme: ThemeConfig::midnight_dark(),
        }
    }
}

/// Full customizable color palette for the WaweFX toolbar, canvas, and soundboard grid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: ThemePreset,
    pub active_user_preset: Option<String>,
    pub bg_main: [u8; 3],
    pub bg_toolbar: [u8; 3],
    pub toolbar_divider: [u8; 3],
    pub toolbar_shadow: [u8; 3],
    pub icon_primary: [u8; 3],
    pub icon_secondary: [u8; 3],
    pub icon_phone: [u8; 3],
    pub gauge_and_track: [u8; 3],
    pub accent_color: [u8; 3],
    pub pad_card_bg: [u8; 3],
    pub pad_card_hover: [u8; 3],
    pub pad_text: [u8; 3],
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self::midnight_dark()
    }
}

impl ThemeConfig {
    /// Cohesive warm cream and sandstone beige palette (`#F4EFE6`, `#EAE2D3`, `#B08968`, `#2B2520`).
    pub fn warm_beige() -> Self {
        Self {
            preset: ThemePreset::WarmBeige,
            active_user_preset: None,
            bg_main: [0xF4, 0xEF, 0xE6],
            bg_toolbar: [0xEA, 0xE2, 0xD3],
            toolbar_divider: [0xCF, 0xC3, 0xB0],
            toolbar_shadow: [0xC4, 0xB6, 0xA0],
            icon_primary: [0x2B, 0x25, 0x20],
            icon_secondary: [0x7A, 0x6F, 0x64],
            icon_phone: [0x2B, 0x25, 0x20],
            gauge_and_track: [0x7A, 0x6F, 0x64],
            accent_color: [0xB0, 0x89, 0x68],
            pad_card_bg: [0xEA, 0xE2, 0xD3],
            pad_card_hover: [0xDF, 0xD5, 0xC3],
            pad_text: [0x2B, 0x25, 0x20],
        }
    }

    /// Deep dark studio palette with crisp slate surfaces and sky-blue accents.
    pub fn midnight_dark() -> Self {
        Self {
            preset: ThemePreset::MidnightDark,
            active_user_preset: None,
            bg_main: [0x18, 0x1B, 0x22],
            bg_toolbar: [0x20, 0x24, 0x2E],
            toolbar_divider: [0x0F, 0x11, 0x17],
            toolbar_shadow: [0x2D, 0x33, 0x42],
            icon_primary: [0xCB, 0xD5, 0xE1],
            icon_secondary: [0x94, 0xA3, 0xB8],
            icon_phone: [0xE2, 0xE8, 0xF0],
            gauge_and_track: [0x64, 0x74, 0x8B],
            accent_color: [0x38, 0xBD, 0xF8],
            pad_card_bg: [0x24, 0x29, 0x36],
            pad_card_hover: [0x2E, 0x34, 0x45],
            pad_text: [0xF1, 0xF5, 0xF9],
        }
    }

    /// Arctic Nord palette with polar night backgrounds and frost cyan accents.
    pub fn nord_frost() -> Self {
        Self {
            preset: ThemePreset::NordFrost,
            active_user_preset: None,
            bg_main: [0x2E, 0x34, 0x40],
            bg_toolbar: [0x3B, 0x42, 0x52],
            toolbar_divider: [0x24, 0x29, 0x33],
            toolbar_shadow: [0x4C, 0x56, 0x6A],
            icon_primary: [0xD8, 0xDE, 0xE9],
            icon_secondary: [0xE5, 0xE9, 0xF0],
            icon_phone: [0xEC, 0xEF, 0xF4],
            gauge_and_track: [0x81, 0xA1, 0xC1],
            accent_color: [0x88, 0xC0, 0xD0],
            pad_card_bg: [0x3B, 0x42, 0x52],
            pad_card_hover: [0x43, 0x4C, 0x5E],
            pad_text: [0xEC, 0xEF, 0xF4],
        }
    }

    /// Constructs a [`ThemeConfig`] from a [`ThemePreset`].
    pub fn from_preset(preset: ThemePreset) -> Self {
        match preset {
            ThemePreset::WarmBeige => Self::warm_beige(),
            ThemePreset::MidnightDark => Self::midnight_dark(),
            ThemePreset::NordFrost => Self::nord_frost(),
            ThemePreset::Custom => {
                let mut custom = Self::midnight_dark();
                custom.preset = ThemePreset::Custom;
                custom.active_user_preset = None;
                custom
            }
        }
    }

    /// Applies one of the built-in presets, replacing all color slots.
    pub fn apply_preset(&mut self, preset: ThemePreset) {
        if preset == ThemePreset::Custom {
            self.preset = ThemePreset::Custom;
            self.active_user_preset = None;
            return;
        }
        *self = Self::from_preset(preset);
    }

    /// Applies a saved user preset palette and marks it as the active named preset.
    pub fn apply_named_preset(&mut self, named: &NamedTheme) {
        let mut next = named.theme.clone();
        next.preset = ThemePreset::Custom;
        next.active_user_preset = Some(named.name.clone());
        *self = next;
    }

    /// Switches `preset` to [`ThemePreset::Custom`] if the current colors no longer match the active preset.
    pub fn mark_custom_if_changed(&mut self) {
        if self.preset == ThemePreset::Custom {
            self.active_user_preset = None;
            return;
        }
        let baseline = Self::from_preset(self.preset);
        if !self.colors_equal(&baseline) {
            self.preset = ThemePreset::Custom;
            self.active_user_preset = None;
        }
    }

    /// Tracks whether edited RGB colors still match either an active user preset or built-in preset.
    pub fn mark_custom_if_changed_with_presets(&mut self, user_presets: &[NamedTheme]) {
        if let Some(ref active_name) = self.active_user_preset {
            let matches_user = user_presets
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(active_name))
                .is_some_and(|p| self.colors_equal(&p.theme));
            if !matches_user {
                self.active_user_preset = None;
                self.preset = ThemePreset::Custom;
            }
            return;
        }
        self.mark_custom_if_changed();
    }

    fn colors_equal(&self, other: &Self) -> bool {
        self.bg_main == other.bg_main
            && self.bg_toolbar == other.bg_toolbar
            && self.toolbar_divider == other.toolbar_divider
            && self.toolbar_shadow == other.toolbar_shadow
            && self.icon_primary == other.icon_primary
            && self.icon_secondary == other.icon_secondary
            && self.icon_phone == other.icon_phone
            && self.gauge_and_track == other.gauge_and_track
            && self.accent_color == other.accent_color
            && self.pad_card_bg == other.pad_card_bg
            && self.pad_card_hover == other.pad_card_hover
            && self.pad_text == other.pad_text
    }
}

/// Converts an `[R, G, B]` array to `egui::Color32`.
pub fn rgb_to_color32(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Identifies which sound pad or global action owns a shortcut binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutOwner<'a> {
    Sound(&'a str),
    StopAll,
    PitchUp,
    PitchDown,
    PitchReset,
}

fn default_true() -> bool {
    true
}

/// Root persistent application configuration serialized to `wawefx_config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub sounds: Vec<SoundItem>,
    pub selected_sound_id: Option<String>,
    pub stop_shortcut: Option<String>,
    pub pitch_up_shortcut: Option<String>,
    pub pitch_down_shortcut: Option<String>,
    pub pitch_reset_shortcut: Option<String>,
    #[serde(default = "default_true")]
    pub allow_overlap: bool,
    pub audio: AudioRoutingConfig,
    pub theme: ThemeConfig,
    pub user_presets: Vec<NamedTheme>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            sounds: Vec::new(),
            selected_sound_id: None,
            stop_shortcut: None,
            pitch_up_shortcut: None,
            pitch_down_shortcut: None,
            pitch_reset_shortcut: None,
            allow_overlap: true,
            audio: AudioRoutingConfig::default(),
            theme: ThemeConfig::default(),
            user_presets: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Returns the managed WaweFX user data directory (`%APPDATA%\WaweFX` on Windows,
    /// `~/Library/Application Support/WaweFX` on macOS), falling back to `<temp>/WaweFX`.
    pub fn data_dir() -> PathBuf {
        Self::resolve_data_dir(dirs::config_dir())
    }

    fn resolve_data_dir(config_dir: Option<PathBuf>) -> PathBuf {
        match config_dir {
            Some(dir) => dir.join(CONFIG_APP_DIR_NAME),
            None => std::env::temp_dir().join(CONFIG_APP_DIR_NAME),
        }
    }

    /// Returns `<data_dir>/sounds`.
    pub fn sounds_dir() -> PathBuf {
        Self::data_dir().join(SOUNDS_SUBDIR)
    }

    /// Returns `<data_dir>/bin`.
    pub fn bin_dir() -> PathBuf {
        Self::data_dir().join(BIN_SUBDIR)
    }

    /// Ensures `<data_dir>`, `<data_dir>/sounds`, `<data_dir>/covers`, and `<data_dir>/bin` exist on disk.
    pub fn ensure_data_dirs() -> anyhow::Result<PathBuf> {
        let base = Self::data_dir();
        Self::ensure_data_dirs_in(&base)?;
        Ok(base)
    }

    fn ensure_data_dirs_in(base_dir: &Path) -> anyhow::Result<()> {
        for sub in [SOUNDS_SUBDIR, COVERS_SUBDIR, BIN_SUBDIR] {
            let dir = base_dir.join(sub);
            fs::create_dir_all(&dir).with_context(|| {
                format!("failed to create managed storage directory {}", dir.display())
            })?;
        }
        Ok(())
    }

    /// Resolves a stored relative asset path against [`AppConfig::data_dir`], or returns
    /// an already-absolute path unchanged.
    pub fn resolve_asset_path(stored_path: &Path) -> PathBuf {
        Self::resolve_asset_path_in(&Self::data_dir(), stored_path)
    }

    fn resolve_asset_path_in(base_dir: &Path, stored_path: &Path) -> PathBuf {
        if stored_path.as_os_str().is_empty() || stored_path.is_absolute() {
            stored_path.to_path_buf()
        } else {
            base_dir.join(stored_path)
        }
    }

    /// Copies an external audio file into `<data_dir>/sounds` and returns its relative path (`sounds/<file>`).
    pub fn import_sound_file(source: &Path) -> anyhow::Result<PathBuf> {
        Self::import_asset_into_dir(&Self::data_dir(), SOUNDS_SUBDIR, source)
    }

    /// Copies an external cover image into `<data_dir>/covers` and returns its relative path (`covers/<file>`).
    pub fn import_cover_file(source: &Path) -> anyhow::Result<PathBuf> {
        Self::import_asset_into_dir(&Self::data_dir(), COVERS_SUBDIR, source)
    }

    /// Saves raw dropped cover image bytes into `<data_dir>/covers` and returns its relative path (`covers/<file>`).
    pub fn import_cover_bytes(bytes: &[u8], hint_name: Option<&str>) -> anyhow::Result<PathBuf> {
        Self::import_cover_bytes_into_dir(&Self::data_dir(), bytes, hint_name)
    }

    fn import_cover_bytes_into_dir(
        base_dir: &Path,
        bytes: &[u8],
        hint_name: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        Self::ensure_data_dirs_in(base_dir)?;
        let decoded = image::load_from_memory(bytes).context("dropped bytes are not a valid image")?;
        let guessed_fmt = image::guess_format(bytes).ok();

        let stem = hint_name
            .and_then(|n| Path::new(n).file_stem())
            .and_then(|s| s.to_str())
            .map(sanitize_filename_stem)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "cover".to_string());
        let short_id = &Uuid::new_v4().simple().to_string()[..8];

        let (ext, encoded_bytes): (&str, Vec<u8>) = match guessed_fmt {
            Some(image::ImageFormat::Png) => ("png", bytes.to_vec()),
            Some(image::ImageFormat::Jpeg) => ("jpg", bytes.to_vec()),
            Some(image::ImageFormat::WebP) => ("webp", bytes.to_vec()),
            Some(image::ImageFormat::Bmp) => ("bmp", bytes.to_vec()),
            Some(image::ImageFormat::Gif) => ("gif", bytes.to_vec()),
            _ => {
                use image::ImageEncoder;
                let rgba = decoded.to_rgba8();
                let mut png_out = Vec::new();
                image::codecs::png::PngEncoder::new(&mut png_out)
                    .write_image(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                        image::ExtendedColorType::Rgba8,
                    )
                    .context("failed to encode dropped image bytes as PNG")?;
                ("png", png_out)
            }
        };

        let file_name = format!("{stem}_{short_id}.{ext}");
        let rel_path = PathBuf::from(COVERS_SUBDIR).join(&file_name);
        let dest_path = base_dir.join(&rel_path);
        fs::write(&dest_path, &encoded_bytes).with_context(|| {
            format!(
                "failed to write dropped cover bytes into '{}'",
                dest_path.display()
            )
        })?;
        Ok(rel_path)
    }

    fn import_asset_into_dir(base_dir: &Path, subdir: &str, source: &Path) -> anyhow::Result<PathBuf> {
        Self::ensure_data_dirs_in(base_dir)?;

        // Already a stored relative managed path under `<base_dir>/<subdir>`
        if source.is_relative()
            && source.starts_with(subdir)
            && let Some(file_name) = source.file_name()
        {
            let rel = PathBuf::from(subdir).join(file_name);
            if base_dir.join(&rel).is_file() {
                return Ok(rel);
            }
        }

        let target_dir = base_dir.join(subdir);
        if let Ok(rel_to_sub) = source.strip_prefix(&target_dir)
            && rel_to_sub.components().count() == 1
            && source.is_file()
        {
            return Ok(PathBuf::from(subdir).join(rel_to_sub));
        }

        if !source.is_file() {
            anyhow::bail!("source file '{}' does not exist", source.display());
        }

        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .map(sanitize_filename_stem)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "asset".to_string());
        let short_id = &Uuid::new_v4().simple().to_string()[..8];
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| !e.trim().is_empty())
            .map(|e| e.to_ascii_lowercase());

        let file_name = match ext {
            Some(ext) => format!("{stem}_{short_id}.{ext}"),
            None => format!("{stem}_{short_id}"),
        };
        let rel_path = PathBuf::from(subdir).join(&file_name);
        let dest_path = base_dir.join(&rel_path);

        fs::copy(source, &dest_path).with_context(|| {
            format!(
                "failed to copy asset '{}' into '{}'",
                source.display(),
                dest_path.display()
            )
        })?;

        Ok(rel_path)
    }

    /// Migrates any valid external audio/cover files referenced in `sounds` into `sounds/` and `covers/`.
    pub fn migrate_external_assets_in(&mut self, base_dir: &Path) -> bool {
        if let Err(err) = Self::ensure_data_dirs_in(base_dir) {
            tracing::warn!("Failed to initialize managed storage directories: {err:#}");
            return false;
        }

        let mut migrated = false;
        for sound in &mut self.sounds {
            if sound.audio_path.is_absolute() && sound.audio_path.is_file() {
                match Self::import_asset_into_dir(base_dir, SOUNDS_SUBDIR, &sound.audio_path) {
                    Ok(rel) => {
                        if sound.audio_path != rel {
                            sound.audio_path = rel;
                            migrated = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Could not migrate audio '{}' for '{}': {err:#}",
                            sound.audio_path.display(),
                            sound.title
                        );
                    }
                }
            }

            if let Some(ref img_path) = sound.image_path.clone()
                && img_path.is_absolute()
                && img_path.is_file()
            {
                match Self::import_asset_into_dir(base_dir, COVERS_SUBDIR, img_path) {
                    Ok(rel) => {
                        if sound.image_path.as_ref() != Some(&rel) {
                            sound.image_path = Some(rel);
                            migrated = true;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Could not migrate cover '{}' for '{}': {err:#}",
                            img_path.display(),
                            sound.title
                        );
                    }
                }
            }
        }
        migrated
    }

    /// Deletes a managed file from `sounds/` or `covers/` if no remaining [`SoundItem`] references it.
    pub fn cleanup_managed_path(&self, candidate: &Path) {
        self.cleanup_managed_path_in(&Self::data_dir(), candidate);
    }

    fn cleanup_managed_path_in(&self, base_dir: &Path, candidate: &Path) {
        if candidate.as_os_str().is_empty() {
            return;
        }
        let resolved_candidate = Self::resolve_asset_path_in(base_dir, candidate);
        let in_sounds = resolved_candidate.starts_with(base_dir.join(SOUNDS_SUBDIR));
        let in_covers = resolved_candidate.starts_with(base_dir.join(COVERS_SUBDIR));
        if (!in_sounds && !in_covers) || !resolved_candidate.is_file() {
            return;
        }

        let still_referenced = self.sounds.iter().any(|s| {
            Self::resolve_asset_path_in(base_dir, &s.audio_path) == resolved_candidate
                || s.image_path
                    .as_deref()
                    .is_some_and(|p| Self::resolve_asset_path_in(base_dir, p) == resolved_candidate)
        });
        if still_referenced {
            return;
        }

        if let Err(err) = fs::remove_file(&resolved_candidate) {
            tracing::warn!(
                "Failed to remove orphaned asset {}: {err}",
                resolved_candidate.display()
            );
        }
    }

    /// Opens the managed WaweFX AppData folder in the OS file browser.
    pub fn open_data_folder() -> anyhow::Result<()> {
        let dir = Self::ensure_data_dirs()?;
        #[cfg(target_os = "windows")]
        {
            std::process::Command::new("explorer")
                .arg(&dir)
                .spawn()
                .with_context(|| format!("failed to open Explorer at {}", dir.display()))?;
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg(&dir)
                .spawn()
                .with_context(|| format!("failed to open Finder at {}", dir.display()))?;
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            std::process::Command::new("xdg-open")
                .arg(&dir)
                .spawn()
                .with_context(|| format!("failed to open file manager at {}", dir.display()))?;
        }
        Ok(())
    }

    /// Returns the platform-appropriate configuration file path:
    /// `<config_dir>/WaweFX/wawefx_config.json` (e.g. `~/Library/Application Support/WaweFX/wawefx_config.json`
    /// on macOS or `%APPDATA%\WaweFX\wawefx_config.json` on Windows), falling back to
    /// `std::env::temp_dir().join("wawefx_config.json")` if no OS config directory is available.
    pub fn config_path() -> PathBuf {
        Self::resolve_config_path(dirs::config_dir())
    }

    fn resolve_config_path(config_dir: Option<PathBuf>) -> PathBuf {
        match config_dir {
            Some(dir) => dir.join(CONFIG_APP_DIR_NAME).join(CONFIG_FILE_NAME),
            None => std::env::temp_dir().join(CONFIG_FILE_NAME),
        }
    }

    /// Ensures managed directories exist and loads configuration from `wawefx_config.json`,
    /// migrating any valid external asset paths into managed `sounds/` and `covers/` folders.
    pub fn load_or_default() -> Self {
        let data_dir = Self::data_dir();
        if let Err(err) = Self::ensure_data_dirs_in(&data_dir) {
            tracing::warn!("Failed to ensure WaweFX data folders: {err:#}");
        }

        let path = Self::config_path();
        if !path.exists() {
            return Self::default();
        }
        let mut cfg = Self::load_from_path(&path).unwrap_or_else(|err| {
            tracing::warn!("Failed to load config from {}: {err:#}. Using defaults.", path.display());
            Self::default()
        });

        if cfg.migrate_external_assets_in(&data_dir)
            && let Err(err) = cfg.save_to_path(&path)
        {
            tracing::warn!("Failed to save migrated config to {}: {err:#}", path.display());
        }
        cfg
    }

    /// Loads and validates configuration from an explicit file path.
    pub fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse JSON config from {}", path.display()))?;
        config.sanitize();
        Ok(config)
    }

    /// Saves configuration to the default `wawefx_config.json` path.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        self.save_to_path(&path)
    }

    /// Serializes and writes configuration to the provided file path, creating parent directories if needed.
    pub fn save_to_path(&self, path: &Path) -> anyhow::Result<()> {
        let mut sanitized = self.clone();
        sanitized.sanitize();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(io_err) = fs::create_dir_all(parent)
        {
            tracing::warn!(
                "Failed to create config directory {}: {io_err}",
                parent.display()
            );
            return Err(anyhow::Error::new(io_err)
                .context(format!("failed to create config parent directory {}", parent.display())));
        }

        let json = serde_json::to_string_pretty(&sanitized)
            .context("failed to serialize AppConfig to JSON")?;
        if let Err(io_err) = fs::write(path, json) {
            tracing::warn!("Failed to write config file {}: {io_err}", path.display());
            return Err(anyhow::Error::new(io_err)
                .context(format!("failed to write config file {}", path.display())));
        }
        Ok(())
    }

    /// Saves the current theme colors as a named user preset in `user_presets`
    /// (updating an existing preset on case-insensitive name match).
    pub fn save_user_preset(&mut self, raw_name: &str) -> bool {
        let name = raw_name.trim().to_string();
        if name.is_empty() {
            return false;
        }
        let mut palette = self.theme.clone();
        palette.preset = ThemePreset::Custom;
        palette.active_user_preset = Some(name.clone());

        if let Some(existing) = self
            .user_presets
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&name))
        {
            existing.name = name.clone();
            existing.theme = palette;
        } else {
            self.user_presets.push(NamedTheme {
                name: name.clone(),
                theme: palette,
            });
        }
        self.theme.preset = ThemePreset::Custom;
        self.theme.active_user_preset = Some(name);
        true
    }

    /// Deletes a saved user preset by index and clears `active_user_preset` if it was selected.
    pub fn delete_user_preset(&mut self, index: usize) -> bool {
        if index >= self.user_presets.len() {
            return false;
        }
        let removed = self.user_presets.remove(index);
        if self
            .theme
            .active_user_preset
            .as_deref()
            .is_some_and(|active| active.eq_ignore_ascii_case(&removed.name))
        {
            self.theme.active_user_preset = None;
            self.theme.preset = ThemePreset::Custom;
        }
        true
    }

    /// Clamps numeric fields to safe runtime bounds and normalizes selection.
    pub fn sanitize(&mut self) {
        self.audio.mic_volume = self.audio.mic_volume.clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
        self.audio.headphones_volume = self
            .audio
            .headphones_volume
            .clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
        self.audio.playback_speed = self
            .audio
            .playback_speed
            .clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
        if !self.audio.pitch_step.is_finite() {
            self.audio.pitch_step = DEFAULT_PITCH_STEP;
        }
        self.audio.pitch_step = self.audio.pitch_step.clamp(MIN_PITCH_STEP, MAX_PITCH_STEP);
        if self.audio.sync_port == 0 {
            self.audio.sync_port = 8765;
        }
        for sound in &mut self.sounds {
            sound.duration_secs = sound.duration_secs.max(0.0);
            sound.volume_trim = sound.volume_trim.clamp(0.0, 2.0);
            let (trim_start, trim_end) = sanitize_trim_range(sound.trim_start, sound.trim_end);
            sound.trim_start = trim_start;
            sound.trim_end = trim_end;
        }
        if let Some(selected) = &self.selected_sound_id
            && !self.sounds.iter().any(|s| &s.id == selected)
        {
            self.selected_sound_id = self.sounds.first().map(|s| s.id.clone());
        }
        for slot in [
            &mut self.stop_shortcut,
            &mut self.pitch_up_shortcut,
            &mut self.pitch_down_shortcut,
            &mut self.pitch_reset_shortcut,
        ] {
            normalize_optional_shortcut(slot);
        }
        for sound in &mut self.sounds {
            normalize_optional_shortcut(&mut sound.shortcut);
        }

        let mut cleaned_presets: Vec<NamedTheme> = Vec::with_capacity(self.user_presets.len());
        for mut preset in std::mem::take(&mut self.user_presets) {
            let trimmed = preset.name.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            preset.name = trimmed.clone();
            preset.theme.preset = ThemePreset::Custom;
            preset.theme.active_user_preset = Some(trimmed.clone());
            if let Some(existing) = cleaned_presets
                .iter_mut()
                .find(|p| p.name.eq_ignore_ascii_case(&trimmed))
            {
                *existing = preset;
            } else {
                cleaned_presets.push(preset);
            }
        }
        self.user_presets = cleaned_presets;
        if let Some(ref active) = self.theme.active_user_preset
            && !self
                .user_presets
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(active))
        {
            self.theme.active_user_preset = None;
        }
    }

    /// Returns a human-readable label of any conflicting sound or global shortcut owner for `shortcut`.
    pub fn shortcut_conflict_label(
        &self,
        shortcut: &str,
        ignore: Option<ShortcutOwner<'_>>,
    ) -> Option<String> {
        let target = crate::hotkeys::canonical_shortcut_key(shortcut)?;
        for sound in &self.sounds {
            if ignore == Some(ShortcutOwner::Sound(sound.id.as_str())) {
                continue;
            }
            if let Some(ref existing) = sound.shortcut
                && crate::hotkeys::canonical_shortcut_key(existing).as_deref()
                    == Some(target.as_str())
            {
                return Some(format!("Sound '{}'", sound.title));
            }
        }
        for (slot, owner, label) in [
            (&self.stop_shortcut, ShortcutOwner::StopAll, "Global Stop All Sounds"),
            (&self.pitch_up_shortcut, ShortcutOwner::PitchUp, "Global Pitch Up"),
            (&self.pitch_down_shortcut, ShortcutOwner::PitchDown, "Global Pitch Down"),
            (&self.pitch_reset_shortcut, ShortcutOwner::PitchReset, "Global Pitch Reset"),
        ] {
            if ignore == Some(owner) {
                continue;
            }
            if let Some(ref existing) = *slot
                && crate::hotkeys::canonical_shortcut_key(existing).as_deref()
                    == Some(target.as_str())
            {
                return Some(label.to_string());
            }
        }
        None
    }

    /// Unbinds `shortcut` from any other sound pad or global action (except `keep`).
    pub fn unbind_shortcut_conflicts(
        &mut self,
        shortcut: &str,
        keep: Option<ShortcutOwner<'_>>,
    ) -> Vec<String> {
        let Some(target) = crate::hotkeys::canonical_shortcut_key(shortcut) else {
            return Vec::new();
        };
        let mut unbound = Vec::new();
        for sound in &mut self.sounds {
            if keep == Some(ShortcutOwner::Sound(sound.id.as_str())) {
                continue;
            }
            if let Some(ref existing) = sound.shortcut
                && crate::hotkeys::canonical_shortcut_key(existing).as_deref()
                    == Some(target.as_str())
            {
                unbound.push(sound.title.clone());
                sound.shortcut = None;
            }
        }
        for (slot, owner, label) in [
            (&mut self.stop_shortcut, ShortcutOwner::StopAll, "Global Stop All Sounds"),
            (&mut self.pitch_up_shortcut, ShortcutOwner::PitchUp, "Global Pitch Up"),
            (&mut self.pitch_down_shortcut, ShortcutOwner::PitchDown, "Global Pitch Down"),
            (&mut self.pitch_reset_shortcut, ShortcutOwner::PitchReset, "Global Pitch Reset"),
        ] {
            if keep == Some(owner) {
                continue;
            }
            if let Some(ref existing) = *slot
                && crate::hotkeys::canonical_shortcut_key(existing).as_deref()
                    == Some(target.as_str())
            {
                unbound.push(label.to_string());
                *slot = None;
            }
        }
        unbound
    }
}

fn normalize_optional_shortcut(slot: &mut Option<String>) {
    *slot = slot
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_midnight_dark() {
        let theme = ThemeConfig::default();
        assert_eq!(theme.preset, ThemePreset::MidnightDark);
        assert_eq!(theme.active_user_preset, None);
        assert_eq!(theme.bg_main, [0x18, 0x1B, 0x22]);
        assert_eq!(theme.bg_toolbar, [0x20, 0x24, 0x2E]);
        assert_eq!(theme.pad_card_bg, [0x24, 0x29, 0x36]);
        assert_eq!(theme.toolbar_divider, [0x0F, 0x11, 0x17]);
        assert_eq!(theme.icon_primary, [0xCB, 0xD5, 0xE1]);
        assert_eq!(theme.pad_text, [0xF1, 0xF5, 0xF9]);
        assert_eq!(theme.gauge_and_track, [0x64, 0x74, 0x8B]);
        assert_eq!(theme.accent_color, [0x38, 0xBD, 0xF8]);
        assert_eq!(
            rgb_to_color32(theme.accent_color),
            Color32::from_rgb(0x38, 0xBD, 0xF8)
        );

        let beige = ThemeConfig::warm_beige();
        assert_eq!(beige.preset, ThemePreset::WarmBeige);
        assert_eq!(beige.bg_main, [0xF4, 0xEF, 0xE6]);
    }

    #[test]
    fn builtin_presets_switch_and_track_custom_edits() {
        assert_eq!(ThemePreset::BUILTIN_PRESETS.len(), 3);
        assert_eq!(ThemePreset::BUILTIN_PRESETS[0], ThemePreset::MidnightDark);

        let mut theme = ThemeConfig::default();
        theme.apply_preset(ThemePreset::WarmBeige);
        assert_eq!(theme.preset, ThemePreset::WarmBeige);
        assert_eq!(theme.bg_main, [0xF4, 0xEF, 0xE6]);

        theme.apply_preset(ThemePreset::NordFrost);
        assert_eq!(theme.preset, ThemePreset::NordFrost);
        assert_eq!(theme.bg_main, [0x2E, 0x34, 0x40]);

        theme.bg_main = [0x11, 0x22, 0x33];
        theme.mark_custom_if_changed();
        assert_eq!(theme.preset, ThemePreset::Custom);
    }

    #[test]
    fn config_json_roundtrip_and_sanitization() {
        let mut cfg = AppConfig::default();
        assert!(cfg.allow_overlap);
        cfg.allow_overlap = false;
        cfg.audio.playback_speed = 9.0;
        cfg.audio.mic_volume = -1.0;
        cfg.audio.headphones_volume = 3.5;

        let mut sound = SoundItem::new(
            "Airhorn",
            PathBuf::from("sounds/airhorn.wav"),
            Some(PathBuf::from("images/airhorn.png")),
            12.4,
        );
        let id = sound.id.clone();
        assert_eq!(sound.trim_start, 0.0);
        assert_eq!(sound.trim_end, 1.0);
        assert_eq!(sound.formatted_duration(), "00:12");
        sound.trim_start = 0.25;
        sound.trim_end = 0.75;
        assert!((sound.trimmed_duration_secs() - 6.2).abs() < 1e-4);
        assert_eq!(sound.formatted_duration(), "00:06");
        cfg.sounds.push(sound);
        cfg.selected_sound_id = Some(id.clone());

        let temp_path = std::env::temp_dir().join(format!("wawefx_test_{}.json", Uuid::new_v4()));
        cfg.save_to_path(&temp_path).expect("save test config");

        let loaded = AppConfig::load_from_path(&temp_path).expect("load test config");
        let _ = fs::remove_file(&temp_path);

        assert!(!loaded.allow_overlap);
        assert_eq!(loaded.audio.playback_speed, MAX_PLAYBACK_SPEED);
        assert_eq!(loaded.audio.mic_volume, MIN_STREAM_VOLUME);
        assert_eq!(loaded.audio.headphones_volume, MAX_STREAM_VOLUME);
        assert_eq!(loaded.selected_sound_id.as_deref(), Some(id.as_str()));
        assert_eq!(loaded.sounds.len(), 1);
        assert_eq!(loaded.sounds[0].title, "Airhorn");
        assert!((loaded.sounds[0].trim_start - 0.25).abs() < 1e-6);
        assert!((loaded.sounds[0].trim_end - 0.75).abs() < 1e-6);

        let (clamped_s, clamped_e) = sanitize_trim_range(0.995, 0.996);
        assert!((clamped_e - clamped_s - MIN_TRIM_GAP).abs() < 1e-6);
        assert!((clamped_e - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resolves_platform_config_path_and_creates_parent_dir() {
        let custom_base = std::env::temp_dir().join(format!("wawefx_cfg_base_{}", Uuid::new_v4()));
        let resolved = AppConfig::resolve_config_path(Some(custom_base.clone()));
        assert_eq!(resolved, custom_base.join("WaweFX").join(CONFIG_FILE_NAME));

        let fallback = AppConfig::resolve_config_path(None);
        assert_eq!(fallback, std::env::temp_dir().join(CONFIG_FILE_NAME));

        let cfg = AppConfig::default();
        cfg.save_to_path(&resolved).expect("save creates nested WaweFX config dir");
        assert!(resolved.exists());
        let _ = fs::remove_dir_all(&custom_base);
    }

    #[test]
    fn shortcuts_load_from_legacy_json_and_unbind_conflicts() {
        let legacy_json = r#"{
            "sounds": [
                {
                    "id": "s1",
                    "title": "Horn",
                    "audio_path": "horn.wav",
                    "duration_secs": 1.0,
                    "volume_trim": 1.0
                }
            ]
        }"#;
        let mut parsed: AppConfig = serde_json::from_str(legacy_json).expect("legacy json parses");
        parsed.sanitize();
        assert!(parsed.allow_overlap);
        assert_eq!(parsed.stop_shortcut, None);
        assert_eq!(parsed.pitch_up_shortcut, None);
        assert_eq!(parsed.pitch_down_shortcut, None);
        assert_eq!(parsed.pitch_reset_shortcut, None);
        assert!(parsed.user_presets.is_empty());
        assert!((parsed.audio.pitch_step - DEFAULT_PITCH_STEP).abs() < 1e-6);
        assert_eq!(parsed.sounds[0].shortcut, None);
        assert_eq!(parsed.sounds[0].trim_start, 0.0);
        assert_eq!(parsed.sounds[0].trim_end, 1.0);

        let mut cfg = AppConfig::default();
        let mut s1 = SoundItem::new("Horn", PathBuf::from("a.wav"), None, 1.0);
        s1.shortcut = Some("Ctrl+Shift+1".to_string());
        let s2 = SoundItem::new("Bell", PathBuf::from("b.wav"), None, 1.0);
        let s2_id = s2.id.clone();
        cfg.sounds.push(s1);
        cfg.sounds.push(s2);
        cfg.stop_shortcut = Some("F9".to_string());
        cfg.pitch_up_shortcut = Some("F10".to_string());
        cfg.pitch_down_shortcut = Some("F11".to_string());
        cfg.pitch_reset_shortcut = Some("F12".to_string());

        let unbound =
            cfg.unbind_shortcut_conflicts("ctrl+shift+1", Some(ShortcutOwner::Sound(&s2_id)));
        assert_eq!(unbound, vec!["Horn".to_string()]);
        assert_eq!(cfg.sounds[0].shortcut, None);

        let unbound_stop = cfg.unbind_shortcut_conflicts("f9", Some(ShortcutOwner::Sound(&s2_id)));
        assert_eq!(unbound_stop, vec!["Global Stop All Sounds".to_string()]);
        assert_eq!(cfg.stop_shortcut, None);

        let unbound_pitch = cfg.unbind_shortcut_conflicts("f10", Some(ShortcutOwner::PitchReset));
        assert_eq!(unbound_pitch, vec!["Global Pitch Up".to_string()]);
        assert_eq!(cfg.pitch_up_shortcut, None);
    }

    #[test]
    fn user_presets_save_apply_overwrite_delete_and_roundtrip() {
        let mut cfg = AppConfig::default();
        cfg.theme.apply_preset(ThemePreset::WarmBeige);
        cfg.theme.accent_color = [0xAA, 0x22, 0x66];
        cfg.theme.mark_custom_if_changed_with_presets(&cfg.user_presets);
        assert_eq!(cfg.theme.preset, ThemePreset::Custom);

        assert!(!cfg.save_user_preset("   "));
        assert!(cfg.save_user_preset("Crimson Lounge"));
        assert_eq!(cfg.user_presets.len(), 1);
        assert_eq!(
            cfg.theme.active_user_preset.as_deref(),
            Some("Crimson Lounge")
        );

        // Editing a color while a named user preset is active transitions back to generic Custom
        cfg.theme.bg_main = [0x10, 0x10, 0x10];
        cfg.theme.mark_custom_if_changed_with_presets(&cfg.user_presets);
        assert_eq!(cfg.theme.active_user_preset, None);
        assert_eq!(cfg.theme.preset, ThemePreset::Custom);

        // Case-insensitive overwrite keeps single entry
        assert!(cfg.save_user_preset("crimson lounge"));
        assert_eq!(cfg.user_presets.len(), 1);
        assert_eq!(cfg.user_presets[0].theme.bg_main, [0x10, 0x10, 0x10]);

        // Switch away and apply saved user preset back
        cfg.theme.apply_preset(ThemePreset::MidnightDark);
        assert_eq!(cfg.theme.active_user_preset, None);
        cfg.theme.apply_named_preset(&cfg.user_presets[0]);
        assert_eq!(cfg.theme.bg_main, [0x10, 0x10, 0x10]);
        assert_eq!(
            cfg.theme.active_user_preset.as_deref(),
            Some("crimson lounge")
        );

        let temp_path =
            std::env::temp_dir().join(format!("wawefx_presets_{}.json", Uuid::new_v4()));
        cfg.save_to_path(&temp_path).expect("save config with user preset");
        let mut loaded = AppConfig::load_from_path(&temp_path).expect("load config with user preset");
        let _ = fs::remove_file(&temp_path);

        assert_eq!(loaded.user_presets.len(), 1);
        assert!(loaded.delete_user_preset(0));
        assert!(loaded.user_presets.is_empty());
        assert_eq!(loaded.theme.active_user_preset, None);
    }

    #[test]
    fn step_playback_speed_clamps_and_avoids_float_drift() {
        let mut speed = 1.0f32;
        for _ in 0..3 {
            speed = step_playback_speed(speed, 0.05);
        }
        assert!((speed - 1.15).abs() < 1e-6);

        for _ in 0..3 {
            speed = step_playback_speed(speed, -0.05);
        }
        assert_eq!(speed, 1.0);

        assert_eq!(step_playback_speed(1.9, 0.5), MAX_PLAYBACK_SPEED);
        assert_eq!(step_playback_speed(0.6, -0.5), MIN_PLAYBACK_SPEED);
    }

    #[test]
    fn manages_self_contained_asset_copy_resolve_and_cleanup() {
        let root = std::env::temp_dir().join(format!("wawefx_managed_{}", Uuid::new_v4()));
        let ext_dir = root.join("Downloads");
        let app_dir = root.join("AppData").join("WaweFX");
        fs::create_dir_all(&ext_dir).unwrap();

        let src_audio = ext_dir.join("air horn!.wav");
        let src_cover = ext_dir.join("cover.png");
        fs::write(&src_audio, b"RIFFdemo").unwrap();
        fs::write(&src_cover, b"\x89PNGdemo").unwrap();

        let mut cfg = AppConfig::default();
        let mut s1 = SoundItem::new("Airhorn", src_audio.clone(), Some(src_cover.clone()), 1.0);
        cfg.sounds.push(s1.clone());

        assert!(cfg.migrate_external_assets_in(&app_dir));
        assert!(cfg.sounds[0].audio_path.starts_with(SOUNDS_SUBDIR));
        assert!(
            cfg.sounds[0]
                .image_path
                .as_ref()
                .is_some_and(|p| p.starts_with(COVERS_SUBDIR))
        );

        let managed_audio = AppConfig::resolve_asset_path_in(&app_dir, &cfg.sounds[0].audio_path);
        let managed_cover = AppConfig::resolve_asset_path_in(
            &app_dir,
            cfg.sounds[0].image_path.as_deref().unwrap(),
        );
        assert!(managed_audio.is_file());
        assert!(managed_cover.is_file());

        // Deleting originals in Downloads leaves managed copies intact
        fs::remove_file(&src_audio).unwrap();
        fs::remove_file(&src_cover).unwrap();
        assert!(managed_audio.is_file());
        assert!(managed_cover.is_file());

        // Add a second pad sharing the same managed audio to test reference-safe cleanup
        s1 = cfg.sounds[0].clone();
        let mut s2 = SoundItem::new("Clone", s1.audio_path.clone(), None, 1.0);
        s2.audio_path = s1.audio_path.clone();
        cfg.sounds.push(s2);

        let removed = cfg.sounds.remove(0);
        cfg.cleanup_managed_path_in(&app_dir, &removed.audio_path);
        if let Some(ref img) = removed.image_path {
            cfg.cleanup_managed_path_in(&app_dir, img);
        }
        assert!(managed_audio.is_file(), "shared audio must remain");
        assert!(!managed_cover.exists(), "unreferenced cover must be deleted");

        let removed2 = cfg.sounds.remove(0);
        cfg.cleanup_managed_path_in(&app_dir, &removed2.audio_path);
        assert!(!managed_audio.exists(), "unreferenced audio must be deleted");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn imports_cover_bytes_into_managed_covers_dir() {
        use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage};

        let root = std::env::temp_dir().join(format!("wawefx_cover_bytes_{}", Uuid::new_v4()));
        let img = RgbaImage::from_pixel(16, 16, Rgba([12, 180, 240, 255]));
        let mut png_bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png_bytes)
            .write_image(img.as_raw(), 16, 16, ExtendedColorType::Rgba8)
            .unwrap();

        let rel = AppConfig::import_cover_bytes_into_dir(&root, &png_bytes, Some("browser-drop.png"))
            .expect("import cover bytes");
        assert!(rel.starts_with(COVERS_SUBDIR));
        assert_eq!(rel.extension().and_then(|e| e.to_str()), Some("png"));
        let full = AppConfig::resolve_asset_path_in(&root, &rel);
        assert!(full.is_file());

        // Animated GIF bytes must be saved with .gif extension and raw bytes intact (not re-encoded to PNG)
        let mut gif_bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif_bytes);
            let f1 = image::Frame::from_parts(
                RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(60, 1),
            );
            let f2 = image::Frame::from_parts(
                RgbaImage::from_pixel(8, 8, Rgba([0, 255, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(80, 1),
            );
            encoder.encode_frames([f1, f2]).expect("encode animated gif");
        }
        let gif_rel =
            AppConfig::import_cover_bytes_into_dir(&root, &gif_bytes, Some("party.gif"))
                .expect("import gif cover bytes");
        assert!(gif_rel.starts_with(COVERS_SUBDIR));
        assert_eq!(gif_rel.extension().and_then(|e| e.to_str()), Some("gif"));
        let gif_full = AppConfig::resolve_asset_path_in(&root, &gif_rel);
        assert_eq!(fs::read(&gif_full).expect("read saved gif"), gif_bytes);

        assert!(AppConfig::import_cover_bytes_into_dir(&root, b"not an image", None).is_err());

        let _ = fs::remove_dir_all(&root);
    }
}

pub fn sanitize_filename_stem(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len().min(32));
    for ch in stem.chars() {
        if out.len() >= 32 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch.is_ascii_whitespace() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}
