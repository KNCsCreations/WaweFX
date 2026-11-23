use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eframe::egui;
use uuid::Uuid;

use crate::audio::AudioEngine;
use crate::config::{AppConfig, SOUNDS_SUBDIR, sanitize_filename_stem};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const YT_DLP_TIMEOUT_SECS: u64 = 120;
const FFMPEG_TIMEOUT_SECS: u64 = 300;

/// Events emitted asynchronously from the background YouTube audio download worker.
#[derive(Debug, Clone)]
pub enum YoutubeDownloadEvent {
    Progress(String),
    Completed {
        audio_path: PathBuf,
        clean_title: String,
        duration_secs: f32,
        waveform_peaks: Arc<Vec<f32>>,
    },
    Failed(String),
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Validates and normalizes a pasted YouTube watch or Shorts URL.
pub fn validate_youtube_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Please paste a YouTube video or Shorts URL.".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("Invalid URL: URLs cannot contain spaces or newlines.".to_string());
    }

    let normalized = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let rest = if let Some(r) = normalized.strip_prefix("https://") {
        r
    } else if let Some(r) = normalized.strip_prefix("http://") {
        r
    } else {
        return Err("Unsupported URL scheme. Paste an https:// YouTube URL.".to_string());
    };

    let (authority, path_and_query) = match rest.find(['/', '?', '#']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };

    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q.split('#').next().unwrap_or("")),
        None => (path_and_query.split('#').next().unwrap_or(""), ""),
    };

    let is_youtu_be = host == "youtu.be" || host == "www.youtu.be";
    let is_youtube = host == "youtube.com" || host.ends_with(".youtube.com");

    if is_youtu_be {
        let video_id = path.trim_start_matches('/').split('/').next().unwrap_or("");
        if video_id.is_empty() {
            return Err("Missing video ID in youtu.be link.".to_string());
        }
        return Ok(normalized);
    }

    if is_youtube {
        let lower_path = path.to_ascii_lowercase();
        if let Some(rest_seg) = lower_path
            .strip_prefix("/shorts/")
            .or_else(|| lower_path.strip_prefix("/live/"))
            .or_else(|| lower_path.strip_prefix("/embed/"))
        {
            let id_seg = rest_seg.split('/').next().unwrap_or("");
            if id_seg.is_empty() {
                return Err("Missing YouTube video ID in URL path.".to_string());
            }
            return Ok(normalized);
        }

        if lower_path == "/watch" || lower_path.is_empty() || lower_path == "/" {
            let has_v = query.split('&').any(|pair| {
                pair.split_once('=')
                    .is_some_and(|(k, v)| k == "v" && !v.trim().is_empty())
            });
            if has_v {
                return Ok(normalized);
            }
            return Err("Missing video ID (`v=...`) in YouTube URL.".to_string());
        }

        return Err(
            "Unsupported YouTube link path. Paste a video (`watch?v=...`, `youtu.be/...`) or Shorts (`/shorts/...`) URL."
                .to_string(),
        );
    }

    Err("Please enter a valid YouTube or Shorts URL (youtube.com or youtu.be).".to_string())
}

/// Video file extensions accepted for automatic audio-track extraction.
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi"];

/// Returns `true` if `path` has a supported video container extension.
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let lower = ext.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&lower.as_str())
        })
}

fn spawn_audio_import_worker<F>(
    thread_name: &str,
    ctx: egui::Context,
    work: F,
) -> Receiver<YoutubeDownloadEvent>
where
    F: FnOnce(&dyn Fn(String)) -> Result<YoutubeDownloadEvent, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();

    let spawn_res = thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            let emit = |ev: YoutubeDownloadEvent| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            };

            match work(&|msg| emit(YoutubeDownloadEvent::Progress(msg))) {
                Ok(completed) => emit(completed),
                Err(err) => emit(YoutubeDownloadEvent::Failed(err)),
            }
        });

    if let Err(err) = spawn_res {
        let (err_tx, err_rx) = mpsc::channel();
        let _ = err_tx.send(YoutubeDownloadEvent::Failed(format!(
            "Could not start audio import worker thread: {err}"
        )));
        return err_rx;
    }

    rx
}

/// Starts a background YouTube audio download + waveform decode worker thread.
pub fn spawn_youtube_download(
    url: String,
    audio: AudioEngine,
    ctx: egui::Context,
) -> Receiver<YoutubeDownloadEvent> {
    spawn_audio_import_worker("wawefx-yt-download", ctx, move |on_progress| {
        download_youtube_audio_sync(&url, &audio, on_progress)
    })
}

/// Starts a background video audio-track extraction + waveform decode worker thread.
pub fn spawn_video_audio_extraction(
    video_path: PathBuf,
    audio: AudioEngine,
    ctx: egui::Context,
) -> Receiver<YoutubeDownloadEvent> {
    spawn_audio_import_worker("wawefx-video-extract", ctx, move |on_progress| {
        extract_video_audio_sync(&video_path, &audio, on_progress)
    })
}

fn download_youtube_audio_sync(
    raw_url: &str,
    audio: &AudioEngine,
    on_progress: &dyn Fn(String),
) -> Result<YoutubeDownloadEvent, String> {
    let url = validate_youtube_url(raw_url)?;
    AppConfig::ensure_data_dirs().map_err(|err| format!("Failed to prepare storage: {err:#}"))?;

    if resolve_yt_dlp_binary().is_none() || resolve_ffmpeg_binary().is_none() {
        on_progress("Downloading yt-dlp & ffmpeg (first-time setup)...".to_string());
    }
    let yt_dlp = ensure_yt_dlp_binary(on_progress)?;

    let sounds_dir = AppConfig::sounds_dir();
    let stage_dir = sounds_dir.join(format!(".yt_tmp_{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&stage_dir)
        .map_err(|err| format!("Failed to create staging directory: {err}"))?;
    let _guard = TempDirGuard(stage_dir.clone());

    let output_template = stage_dir
        .join("%(title).80B [%(id)s].%(ext)s")
        .to_string_lossy()
        .into_owned();
    let child_path = build_augmented_path(&AppConfig::bin_dir());

    let downloaded_file = stage_youtube_audio(
        &yt_dlp,
        &stage_dir,
        &output_template,
        &url,
        &child_path,
        on_progress,
    )?;

    let staged_stem = downloaded_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("YouTube Sound");
    let clean_title = clean_title_from_stem(staged_stem);

    let ext = downloaded_file
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.trim().is_empty())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "mp3".to_string());

    let sanitized_base = {
        let s = sanitize_filename_stem(&clean_title);
        if s.is_empty() {
            "youtube_sound".to_string()
        } else {
            s
        }
    };
    let short_id = &Uuid::new_v4().simple().to_string()[..8];
    let final_filename = format!("{sanitized_base}_{short_id}.{ext}");
    let rel_path = PathBuf::from(SOUNDS_SUBDIR).join(&final_filename);
    let final_path = sounds_dir.join(&final_filename);

    move_or_copy_file(&downloaded_file, &final_path).map_err(|err| {
        format!(
            "Failed to save downloaded audio to '{}': {err}",
            final_path.display()
        )
    })?;
    clear_dir_files(&stage_dir);

    probe_extracted_audio(audio, rel_path, &final_path, clean_title, on_progress)
}

fn probe_extracted_audio(
    audio: &AudioEngine,
    rel_path: PathBuf,
    final_path: &Path,
    clean_title: String,
    on_progress: &dyn Fn(String),
) -> Result<YoutubeDownloadEvent, String> {
    on_progress("Analyzing waveform & duration...".to_string());
    match audio.probe_audio_with_peaks(&rel_path) {
        Ok((duration_secs, waveform_peaks)) => Ok(YoutubeDownloadEvent::Completed {
            audio_path: rel_path,
            clean_title,
            duration_secs,
            waveform_peaks,
        }),
        Err(err) => {
            let _ = fs::remove_file(final_path);
            Err(format!("Extracted audio could not be decoded: {err:#}"))
        }
    }
}

fn build_ffmpeg_extract_mp3_args(input_video: &Path, output_mp3: &Path) -> Vec<OsString> {
    vec![
        OsString::from("-y"),
        OsString::from("-i"),
        input_video.as_os_str().to_os_string(),
        OsString::from("-vn"),
        OsString::from("-acodec"),
        OsString::from("libmp3lame"),
        OsString::from("-q:a"),
        OsString::from("2"),
        output_mp3.as_os_str().to_os_string(),
    ]
}

fn run_ffmpeg_extract_mp3(
    ffmpeg_bin: &Path,
    input_video: &Path,
    output_mp3: &Path,
    child_path: &OsString,
) -> Result<Output, String> {
    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(build_ffmpeg_extract_mp3_args(input_video, output_mp3))
        .stdin(Stdio::null())
        .env("PATH", child_path);
    apply_no_window(&mut cmd);

    cmd.output()
        .map_err(|err| format!("Failed to launch ffmpeg ('{}'): {err}", ffmpeg_bin.display()))
}

fn format_process_stderr(output: &Output, tool_name: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last_non_empty = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty());
    match last_non_empty {
        Some(line) => format!("{tool_name} failed: {line}"),
        None => format!(
            "{tool_name} failed with exit code {:?}",
            output.status.code()
        ),
    }
}

fn extract_video_audio_sync(
    video_path: &Path,
    audio: &AudioEngine,
    on_progress: &dyn Fn(String),
) -> Result<YoutubeDownloadEvent, String> {
    if !video_path.is_file() {
        return Err(format!("Video file '{}' was not found.", video_path.display()));
    }

    AppConfig::ensure_data_dirs().map_err(|err| format!("Failed to prepare storage: {err:#}"))?;

    let raw_stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Video Sound");
    let clean_title = raw_stem.to_string();
    let sanitized_base = {
        let s = sanitize_filename_stem(&clean_title);
        if s.is_empty() {
            "video_sound".to_string()
        } else {
            s
        }
    };
    let short_id = Uuid::new_v4().simple().to_string();
    let short_id = &short_id[..8];
    let sounds_dir = AppConfig::sounds_dir();

    if resolve_ffmpeg_binary().is_none() {
        on_progress("Downloading ffmpeg (first-time setup)...".to_string());
    }

    let (rel_path, final_path) = stage_video_audio(
        video_path,
        &sounds_dir,
        &sanitized_base,
        short_id,
        on_progress,
    )?;

    probe_extracted_audio(audio, rel_path, &final_path, clean_title, on_progress)
}

fn try_extract_video_with_ffmpeg(
    video_path: &Path,
    sounds_dir: &Path,
    sanitized_base: &str,
    short_id: &str,
    on_progress: &dyn Fn(String),
) -> Result<(PathBuf, PathBuf), String> {
    let ffmpeg_bin = ensure_ffmpeg_binary(on_progress)?;
    let final_filename = format!("{sanitized_base}_{short_id}.mp3");
    let rel_path = PathBuf::from(SOUNDS_SUBDIR).join(&final_filename);
    let final_path = sounds_dir.join(&final_filename);
    let child_path = build_augmented_path(&AppConfig::bin_dir());

    on_progress("Extracting audio from video...".to_string());
    let out = run_ffmpeg_extract_mp3(&ffmpeg_bin, video_path, &final_path, &child_path)?;
    if !out.status.success()
        || !final_path.is_file()
        || fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0) == 0
    {
        let _ = fs::remove_file(&final_path);
        return Err(format_process_stderr(&out, "ffmpeg"));
    }

    Ok((rel_path, final_path))
}

#[cfg(target_os = "macos")]
fn stage_video_audio(
    video_path: &Path,
    sounds_dir: &Path,
    sanitized_base: &str,
    short_id: &str,
    on_progress: &dyn Fn(String),
) -> Result<(PathBuf, PathBuf), String> {
    if let Ok(paths) = try_extract_video_with_ffmpeg(
        video_path,
        sounds_dir,
        sanitized_base,
        short_id,
        on_progress,
    ) {
        return Ok(paths);
    }

    on_progress("Extracting audio via macOS afconvert...".to_string());
    let final_filename = format!("{sanitized_base}_{short_id}.wav");
    let rel_path = PathBuf::from(SOUNDS_SUBDIR).join(&final_filename);
    let final_path = sounds_dir.join(&final_filename);
    run_afconvert_to_wav(video_path, &final_path)?;
    Ok((rel_path, final_path))
}

#[cfg(not(target_os = "macos"))]
fn stage_video_audio(
    video_path: &Path,
    sounds_dir: &Path,
    sanitized_base: &str,
    short_id: &str,
    on_progress: &dyn Fn(String),
) -> Result<(PathBuf, PathBuf), String> {
    try_extract_video_with_ffmpeg(video_path, sounds_dir, sanitized_base, short_id, on_progress)
}

#[cfg(target_os = "macos")]
fn stage_youtube_audio(
    yt_dlp: &Path,
    stage_dir: &Path,
    output_template: &str,
    url: &str,
    child_path: &OsString,
    on_progress: &dyn Fn(String),
) -> Result<PathBuf, String> {
    if let Ok(ffmpeg_bin) = ensure_ffmpeg_binary(on_progress) {
        on_progress("Downloading & converting audio to MP3...".to_string());
        if let Ok(out) = run_yt_dlp_mp3(yt_dlp, &ffmpeg_bin, output_template, url, child_path)
            && out.status.success()
        {
            return find_first_file_in_dir(stage_dir);
        }
        clear_dir_files(stage_dir);
    }

    run_macos_afconvert_fallback(
        yt_dlp,
        stage_dir,
        output_template,
        url,
        child_path,
        on_progress,
    )
}

#[cfg(not(target_os = "macos"))]
fn stage_youtube_audio(
    yt_dlp: &Path,
    stage_dir: &Path,
    output_template: &str,
    url: &str,
    child_path: &OsString,
    on_progress: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let ffmpeg_bin = ensure_ffmpeg_binary(on_progress)?;
    on_progress("Downloading & converting audio to MP3...".to_string());

    let out = run_yt_dlp_mp3(yt_dlp, &ffmpeg_bin, output_template, url, child_path)?;
    if !out.status.success() {
        clear_dir_files(stage_dir);
        return Err(format_yt_dlp_error(&out));
    }

    find_first_file_in_dir(stage_dir)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn build_afconvert_wav_path(input_path: &Path) -> PathBuf {
    if input_path.file_stem().is_some() {
        input_path.with_extension("wav")
    } else {
        input_path.join("audio.wav")
    }
}

#[cfg(target_os = "macos")]
fn run_afconvert_to_wav(input_path: &Path, wav_path: &Path) -> Result<(), String> {
    let _ = fs::remove_file(wav_path);

    let out = Command::new("/usr/bin/afconvert")
        .args(["-f", "WAVE", "-d", "LEI16"])
        .arg(input_path)
        .arg(wav_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("Failed to run /usr/bin/afconvert: {err}"))?;

    if !out.status.success() || !wav_path.is_file() {
        let _ = fs::remove_file(wav_path);
        return Err(format_process_stderr(&out, "macOS afconvert"));
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn convert_audio_with_afconvert(input_path: &Path) -> Result<PathBuf, String> {
    if input_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    {
        return Ok(input_path.to_path_buf());
    }

    let wav_path = build_afconvert_wav_path(input_path);
    run_afconvert_to_wav(input_path, &wav_path)?;
    let _ = fs::remove_file(input_path);
    Ok(wav_path)
}

#[cfg(target_os = "macos")]
fn run_macos_afconvert_fallback(
    yt_dlp: &Path,
    stage_dir: &Path,
    output_template: &str,
    url: &str,
    child_path: &OsString,
    on_progress: &dyn Fn(String),
) -> Result<PathBuf, String> {
    on_progress("Downloading audio & converting via macOS afconvert...".to_string());
    let out = run_yt_dlp_m4a(yt_dlp, output_template, url, child_path)?;
    if !out.status.success() {
        clear_dir_files(stage_dir);
        return Err(format_yt_dlp_error(&out));
    }

    let downloaded_m4a = find_first_file_in_dir(stage_dir)?;
    convert_audio_with_afconvert(&downloaded_m4a)
}

fn run_yt_dlp_cmd(
    yt_dlp: &Path,
    args: &[&OsStr],
    child_path: &OsString,
) -> Result<Output, String> {
    let mut cmd = Command::new(yt_dlp);
    cmd.args(args).env("PATH", child_path);
    apply_no_window(&mut cmd);

    cmd.output()
        .map_err(|err| format!("Failed to launch yt-dlp ('{}'): {err}", yt_dlp.display()))
}

fn run_yt_dlp_mp3(
    yt_dlp: &Path,
    ffmpeg_bin: &Path,
    output_template: &str,
    url: &str,
    child_path: &OsString,
) -> Result<Output, String> {
    let ffmpeg_loc = ffmpeg_bin.parent().unwrap_or(ffmpeg_bin);
    run_yt_dlp_cmd(
        yt_dlp,
        &[
            OsStr::new("--no-playlist"),
            OsStr::new("--no-progress"),
            OsStr::new("-x"),
            OsStr::new("--audio-format"),
            OsStr::new("mp3"),
            OsStr::new("--audio-quality"),
            OsStr::new("0"),
            OsStr::new("--ffmpeg-location"),
            ffmpeg_loc.as_os_str(),
            OsStr::new("-o"),
            OsStr::new(output_template),
            OsStr::new("--"),
            OsStr::new(url),
        ],
        child_path,
    )
}

#[cfg(target_os = "macos")]
fn run_yt_dlp_m4a(
    yt_dlp: &Path,
    output_template: &str,
    url: &str,
    child_path: &OsString,
) -> Result<Output, String> {
    run_yt_dlp_cmd(
        yt_dlp,
        &[
            OsStr::new("--no-playlist"),
            OsStr::new("--no-progress"),
            OsStr::new("-f"),
            OsStr::new("bestaudio[ext=m4a]/bestaudio"),
            OsStr::new("-o"),
            OsStr::new(output_template),
            OsStr::new("--"),
            OsStr::new(url),
        ],
        child_path,
    )
}

#[cfg(target_os = "windows")]
fn apply_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_no_window(_cmd: &mut Command) {}

fn clear_dir_files(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn find_first_file_in_dir(dir: &Path) -> Result<PathBuf, String> {
    let entries = fs::read_dir(dir)
        .map_err(|err| format!("Failed to inspect download directory: {err}"))?;
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            !matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("part" | "ytdl")
            )
        })
        .collect();
    files.sort_by_key(|p| {
        let is_mp3 = p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp3"));
        (!is_mp3, p.clone())
    });
    files.into_iter().next().ok_or_else(|| {
        "yt-dlp finished without producing an audio file. Check the URL and try again.".to_string()
    })
}

fn move_or_copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(src, dst)?;
            let _ = fs::remove_file(src);
            Ok(())
        }
    }
}

/// Builds a prioritized directory list for resolving helper binaries (`WaweFX/bin/` first,
/// followed by `PATH` and standard macOS/Linux Homebrew/system locations).
fn binary_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![AppConfig::bin_dir()];
    if let Some(path_var) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path_var));
    }
    #[cfg(not(target_os = "windows"))]
    {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            let p = PathBuf::from(extra);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }
    dirs
}

fn build_augmented_path(bin_dir: &Path) -> OsString {
    let mut paths = vec![bin_dir.to_path_buf()];
    #[cfg(not(target_os = "windows"))]
    {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
            let p = PathBuf::from(extra);
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    if let Some(existing) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&existing) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    std::env::join_paths(paths).unwrap_or_default()
}

fn resolve_binary_in_dirs(names: &[&str], dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolves an existing `yt-dlp` executable in `WaweFX/bin/` or `PATH`/Homebrew directories.
pub fn resolve_yt_dlp_binary() -> Option<PathBuf> {
    let dirs = binary_search_dirs();
    #[cfg(target_os = "windows")]
    let names = &["yt-dlp.exe", "yt-dlp"];
    #[cfg(not(target_os = "windows"))]
    let names = &["yt-dlp", "yt-dlp_macos"];
    resolve_binary_in_dirs(names, &dirs)
}

/// Resolves an existing `ffmpeg` executable in `WaweFX/bin/` or `PATH`/Homebrew directories.
pub fn resolve_ffmpeg_binary() -> Option<PathBuf> {
    let dirs = binary_search_dirs();
    #[cfg(target_os = "windows")]
    let names = &["ffmpeg.exe", "ffmpeg"];
    #[cfg(not(target_os = "windows"))]
    let names = &["ffmpeg"];
    resolve_binary_in_dirs(names, &dirs)
}

fn platform_yt_dlp_download_spec() -> (&'static str, &'static str) {
    #[cfg(target_os = "windows")]
    {
        (
            "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
            "yt-dlp.exe",
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
            "yt-dlp",
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        (
            "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp",
            "yt-dlp",
        )
    }
}

/// Locates `yt-dlp` on the system or automatically downloads the official release into `WaweFX/bin/`.
fn ensure_yt_dlp_binary(on_progress: &dyn Fn(String)) -> Result<PathBuf, String> {
    if let Some(existing) = resolve_yt_dlp_binary() {
        return Ok(existing);
    }

    let bin_dir = AppConfig::bin_dir();
    fs::create_dir_all(&bin_dir)
        .map_err(|err| format!("Failed to create '{}': {err}", bin_dir.display()))?;

    let (download_url, dest_name) = platform_yt_dlp_download_spec();
    let final_path = bin_dir.join(dest_name);
    let part_path = bin_dir.join(format!("{dest_name}.part"));
    let _ = fs::remove_file(&part_path);

    on_progress("Downloading yt-dlp & ffmpeg (first-time setup)...".to_string());

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(YT_DLP_TIMEOUT_SECS))
        .build();
    let response = agent
        .get(download_url)
        .set("User-Agent", "WaweFX")
        .call()
        .map_err(|err| {
            format!("yt-dlp was not found and automatic download failed (check internet connection): {err}")
        })?;

    if response.status() != 200 {
        return Err(format!(
            "Failed to download yt-dlp helper (HTTP {}).",
            response.status()
        ));
    }

    let copy_res = (|| -> io::Result<()> {
        let mut reader = response.into_reader();
        let mut out_file = File::create(&part_path)?;
        io::copy(&mut reader, &mut out_file)?;
        out_file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = copy_res {
        let _ = fs::remove_file(&part_path);
        return Err(format!("Failed writing yt-dlp helper to disk: {err}"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = fs::set_permissions(&part_path, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&part_path);
            return Err(format!("Failed setting executable bit on yt-dlp: {err}"));
        }
    }

    if let Err(err) = fs::rename(&part_path, &final_path) {
        let _ = fs::remove_file(&part_path);
        return Err(format!("Failed finalizing yt-dlp binary: {err}"));
    }

    Ok(final_path)
}

#[cfg(target_os = "windows")]
fn windows_ffmpeg_download_spec() -> (
    &'static [&'static str],
    &'static [&'static str],
    &'static str,
) {
    (
        &["https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip"],
        &["ffmpeg.exe", "ffprobe.exe"],
        "ffmpeg.exe",
    )
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_ffmpeg_download_urls_for_arch(arch: &str) -> &'static [&'static str] {
    match arch {
        "aarch64" => &[
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-arm64",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-arm64",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-arm64.gz",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-arm64.gz",
        ],
        _ => &[
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-x64",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-x64",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-x64.gz",
            "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-x64.gz",
        ],
    }
}

/// Extracts matching binary filenames from a `.zip` archive into `bin_dir`, returning the primary binary path.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn extract_binaries_from_zip<R: Read + Seek>(
    reader: R,
    bin_dir: &Path,
    target_names: &[&str],
    primary_name: &str,
) -> Result<PathBuf, String> {
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|err| format!("Failed to read downloaded ffmpeg zip archive: {err}"))?;

    for idx in 0..archive.len() {
        let mut entry = archive
            .by_index(idx)
            .map_err(|err| format!("Failed to read zip entry #{idx}: {err}"))?;
        if entry.is_dir() {
            continue;
        }
        let raw_name = entry.name();
        let base_name = raw_name.rsplit(['/', '\\']).next().unwrap_or(raw_name);
        let Some(&canonical_name) = target_names
            .iter()
            .find(|&&candidate| candidate.eq_ignore_ascii_case(base_name))
        else {
            continue;
        };

        let part_path = bin_dir.join(format!("{canonical_name}.part"));
        let final_path = bin_dir.join(canonical_name);
        let _ = fs::remove_file(&part_path);

        let write_res = (|| -> io::Result<()> {
            let mut out_file = File::create(&part_path)?;
            io::copy(&mut entry, &mut out_file)?;
            out_file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_res {
            let _ = fs::remove_file(&part_path);
            return Err(format!("Failed extracting '{canonical_name}': {err}"));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = fs::set_permissions(&part_path, fs::Permissions::from_mode(0o755)) {
                let _ = fs::remove_file(&part_path);
                return Err(format!(
                    "Failed setting executable permissions on '{canonical_name}': {err}"
                ));
            }
        }

        let _ = fs::remove_file(&final_path);
        if let Err(err) = fs::rename(&part_path, &final_path) {
            let _ = fs::remove_file(&part_path);
            return Err(format!("Failed finalizing '{canonical_name}': {err}"));
        }
    }

    let primary_path = bin_dir.join(primary_name);
    if primary_path.is_file() {
        Ok(primary_path)
    } else {
        Err(format!(
            "Downloaded ffmpeg archive did not contain '{primary_name}'."
        ))
    }
}

fn verify_ffmpeg_runs(ffmpeg_path: &Path) -> bool {
    let mut cmd = Command::new(ffmpeg_path);
    cmd.arg("-version").stdin(Stdio::null());
    apply_no_window(&mut cmd);
    matches!(cmd.output(), Ok(out) if out.status.success())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn download_to_path(agent: &ureq::Agent, url: &str, dest_path: &Path) -> Result<(), String> {
    let _ = fs::remove_file(dest_path);
    match agent.get(url).set("User-Agent", "WaweFX").call() {
        Ok(response) if response.status() == 200 => {
            let copy_res = (|| -> io::Result<()> {
                let mut reader = response.into_reader();
                let mut out_file = File::create(dest_path)?;
                io::copy(&mut reader, &mut out_file)?;
                out_file.sync_all()?;
                Ok(())
            })();
            copy_res.map_err(|err| format!("failed writing download: {err}"))
        }
        Ok(response) => Err(format!("HTTP {}", response.status())),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(target_os = "windows")]
fn download_windows_ffmpeg_binary(
    bin_dir: &Path,
    agent: &ureq::Agent,
) -> Result<PathBuf, String> {
    let (urls, target_names, primary_name) = windows_ffmpeg_download_spec();
    let zip_tmp = bin_dir.join("ffmpeg_download.zip.part");
    let mut last_err = String::from("no download URLs configured");
    let mut downloaded_ok = false;

    for &url in urls {
        match download_to_path(agent, url, &zip_tmp) {
            Ok(()) => {
                downloaded_ok = true;
                break;
            }
            Err(err) => last_err = err,
        }
    }

    if !downloaded_ok {
        let _ = fs::remove_file(&zip_tmp);
        return Err(format!(
            "ffmpeg was not found and automatic download failed ({last_err}). Check your internet connection or install ffmpeg."
        ));
    }

    let extract_res = (|| -> Result<PathBuf, String> {
        let file = File::open(&zip_tmp)
            .map_err(|err| format!("Failed to open downloaded ffmpeg zip: {err}"))?;
        extract_binaries_from_zip(
            io::BufReader::new(file),
            bin_dir,
            target_names,
            primary_name,
        )
    })();

    let _ = fs::remove_file(&zip_tmp);
    let primary_path = extract_res?;
    if !verify_ffmpeg_runs(&primary_path) {
        let _ = fs::remove_file(&primary_path);
        return Err("Downloaded ffmpeg binary failed verification (-version).".to_string());
    }
    Ok(primary_path)
}

#[cfg(target_os = "macos")]
fn maybe_decompress_gzip_file(part_path: &Path) -> Result<(), String> {
    let mut magic = [0u8; 2];
    let is_gzip = File::open(part_path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1f, 0x8b];
    if !is_gzip {
        return Ok(());
    }

    let gz_path = part_path.with_extension("part.gz");
    let _ = fs::remove_file(&gz_path);
    fs::rename(part_path, &gz_path)
        .map_err(|err| format!("Failed staging gzipped ffmpeg: {err}"))?;

    let out = Command::new("/usr/bin/gzip")
        .args(["-df"])
        .arg(&gz_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            let _ = fs::remove_file(&gz_path);
            format!("Failed to run /usr/bin/gzip: {err}")
        })?;

    if !out.status.success() || !part_path.is_file() {
        let _ = fs::remove_file(&gz_path);
        let _ = fs::remove_file(part_path);
        return Err("Failed decompressing downloaded gzipped ffmpeg.".to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn download_macos_ffmpeg_binary(
    bin_dir: &Path,
    agent: &ureq::Agent,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let urls = macos_ffmpeg_download_urls_for_arch(std::env::consts::ARCH);
    let part_path = bin_dir.join("ffmpeg.part");
    let final_path = bin_dir.join("ffmpeg");
    let mut last_err = String::from("no download URLs configured");

    for &url in urls {
        if let Err(err) = download_to_path(agent, url, &part_path) {
            let _ = fs::remove_file(&part_path);
            last_err = err;
            continue;
        }

        if let Err(err) = maybe_decompress_gzip_file(&part_path) {
            last_err = err;
            continue;
        }

        let file_len = fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
        if file_len < 1_000_000 {
            let _ = fs::remove_file(&part_path);
            last_err = format!("downloaded file too small ({file_len} bytes)");
            continue;
        }

        if let Err(err) = fs::set_permissions(&part_path, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&part_path);
            last_err = format!("failed setting executable permissions: {err}");
            continue;
        }

        let _ = fs::remove_file(&final_path);
        if let Err(err) = fs::rename(&part_path, &final_path) {
            let _ = fs::remove_file(&part_path);
            last_err = format!("failed finalizing ffmpeg binary: {err}");
            continue;
        }

        if verify_ffmpeg_runs(&final_path) {
            return Ok(final_path);
        }

        let _ = fs::remove_file(&final_path);
        last_err = "downloaded ffmpeg failed -version verification".to_string();
    }

    Err(format!(
        "ffmpeg was not found and automatic macOS download failed ({last_err})."
    ))
}

/// Locates `ffmpeg` in `WaweFX/bin/` or system `PATH`, or downloads a standalone static build into `WaweFX/bin/`.
fn ensure_ffmpeg_binary(on_progress: &dyn Fn(String)) -> Result<PathBuf, String> {
    if let Some(existing) = resolve_ffmpeg_binary() {
        if verify_ffmpeg_runs(&existing) {
            return Ok(existing);
        }
        if existing.starts_with(AppConfig::bin_dir()) {
            let _ = fs::remove_file(&existing);
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = on_progress;
        return Err(
            "ffmpeg was not found in WaweFX/bin/ or PATH. Please install ffmpeg via your package manager."
                .to_string(),
        );
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let bin_dir = AppConfig::bin_dir();
        fs::create_dir_all(&bin_dir)
            .map_err(|err| format!("Failed to create '{}': {err}", bin_dir.display()))?;

        on_progress("Downloading yt-dlp & ffmpeg (first-time setup)...".to_string());

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(FFMPEG_TIMEOUT_SECS))
            .build();

        #[cfg(target_os = "windows")]
        {
            download_windows_ffmpeg_binary(&bin_dir, &agent)
        }
        #[cfg(target_os = "macos")]
        {
            download_macos_ffmpeg_binary(&bin_dir, &agent)
        }
    }
}

/// Strips a trailing ` [<youtube_id>]` suffix from a downloaded filename stem.
pub fn clean_title_from_stem(stem: &str) -> String {
    let trimmed = stem.trim();
    if let Some(without_close) = trimmed.strip_suffix(']')
        && let Some(open_idx) = without_close.rfind(" [")
    {
        let id_part = &without_close[open_idx + 2..];
        let looks_like_id = !id_part.is_empty()
            && id_part.len() <= 16
            && id_part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if looks_like_id {
            let prefix = without_close[..open_idx].trim();
            if !prefix.is_empty() {
                return prefix.to_string();
            }
        }
    }
    if trimmed.is_empty() {
        "YouTube Sound".to_string()
    } else {
        trimmed.to_string()
    }
}

fn format_yt_dlp_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines().rev() {
        let trimmed = line.trim();
        if let Some(msg) = trimmed.strip_prefix("ERROR:") {
            let clean = msg.trim();
            if !clean.is_empty() {
                return format!("Download failed: {clean}");
            }
        }
    }
    let last_non_empty = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty());
    match last_non_empty {
        Some(line) => format!("Download failed: {line}"),
        None => format!("yt-dlp failed with exit code {:?}", output.status.code()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;

    use uuid::Uuid;
    use zip::write::SimpleFileOptions;

    use super::{
        build_afconvert_wav_path, build_ffmpeg_extract_mp3_args, clean_title_from_stem,
        extract_binaries_from_zip, is_video_file, macos_ffmpeg_download_urls_for_arch,
        resolve_binary_in_dirs, validate_youtube_url,
    };

    #[test]
    fn validates_standard_watch_shorts_and_short_youtube_urls() {
        assert_eq!(
            validate_youtube_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Ok("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            validate_youtube_url("youtube.com/watch?v=dQw4w9WgXcQ&t=5s"),
            Ok("https://youtube.com/watch?v=dQw4w9WgXcQ&t=5s".to_string())
        );
        assert_eq!(
            validate_youtube_url("https://youtu.be/dQw4w9WgXcQ?si=test"),
            Ok("https://youtu.be/dQw4w9WgXcQ?si=test".to_string())
        );
        assert_eq!(
            validate_youtube_url("https://www.youtube.com/shorts/abcdefghijk"),
            Ok("https://www.youtube.com/shorts/abcdefghijk".to_string())
        );
        assert_eq!(
            validate_youtube_url("https://m.youtube.com/shorts/xyz_123-ab?feature=share"),
            Ok("https://m.youtube.com/shorts/xyz_123-ab?feature=share".to_string())
        );
    }

    #[test]
    fn rejects_empty_non_youtube_or_missing_id_urls() {
        assert!(validate_youtube_url("").is_err());
        assert!(validate_youtube_url("   ").is_err());
        assert!(validate_youtube_url("https://vimeo.com/12345").is_err());
        assert!(validate_youtube_url("https://youtube.com/watch").is_err());
        assert!(validate_youtube_url("https://youtu.be/").is_err());
        assert!(validate_youtube_url("https://youtube.com/shorts/").is_err());
        assert!(validate_youtube_url("javascript:alert(1)").is_err());
        assert!(validate_youtube_url("https://youtube.com/watch?v=abc def").is_err());
    }

    #[test]
    fn cleans_video_title_from_yt_dlp_output_stem() {
        assert_eq!(
            clean_title_from_stem("Airhorn MLG Sound Effect [dQw4w9WgXcQ]"),
            "Airhorn MLG Sound Effect"
        );
        assert_eq!(
            clean_title_from_stem("Song [Official Audio] [abc_123-XYZ]"),
            "Song [Official Audio]"
        );
        assert_eq!(clean_title_from_stem("Custom File"), "Custom File");
        assert_eq!(clean_title_from_stem(""), "YouTube Sound");
    }

    #[test]
    fn resolves_binaries_in_priority_order_preferring_managed_bin_dir() {
        let root = std::env::temp_dir().join(format!("wawefx_bin_res_{}", Uuid::new_v4()));
        let managed = root.join("bin");
        let sys = root.join("sys");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&sys).unwrap();

        let sys_tool = sys.join("yt-dlp.exe");
        let managed_tool = managed.join("yt-dlp.exe");
        fs::write(&sys_tool, b"sys").unwrap();

        let search_dirs: Vec<PathBuf> = vec![managed.clone(), sys.clone()];
        assert_eq!(
            resolve_binary_in_dirs(&["yt-dlp.exe", "yt-dlp"], &search_dirs),
            Some(sys_tool)
        );

        fs::write(&managed_tool, b"managed").unwrap();
        assert_eq!(
            resolve_binary_in_dirs(&["yt-dlp.exe", "yt-dlp"], &search_dirs),
            Some(managed_tool)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extracts_ffmpeg_binary_from_nested_zip_archive() {
        let mut zip_buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip_buf));
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer
                .start_file("ffmpeg-master-latest-win64-gpl/LICENSE.txt", opts)
                .unwrap();
            writer.write_all(b"GPLv3").unwrap();

            writer
                .start_file("ffmpeg-master-latest-win64-gpl/bin/ffmpeg.exe", opts)
                .unwrap();
            writer.write_all(b"FAKE_FFMPEG_BINARY").unwrap();

            writer
                .start_file("ffmpeg-master-latest-win64-gpl/bin/ffprobe.exe", opts)
                .unwrap();
            writer.write_all(b"FAKE_FFPROBE_BINARY").unwrap();

            writer.finish().unwrap();
        }

        let bin_dir =
            std::env::temp_dir().join(format!("wawefx_ffmpeg_zip_{}", Uuid::new_v4()));
        fs::create_dir_all(&bin_dir).unwrap();

        let extracted = extract_binaries_from_zip(
            Cursor::new(&zip_buf),
            &bin_dir,
            &["ffmpeg.exe", "ffprobe.exe"],
            "ffmpeg.exe",
        )
        .expect("extract ffmpeg from zip");

        assert_eq!(extracted, bin_dir.join("ffmpeg.exe"));
        assert_eq!(fs::read(&extracted).unwrap(), b"FAKE_FFMPEG_BINARY");
        assert_eq!(
            fs::read(bin_dir.join("ffprobe.exe")).unwrap(),
            b"FAKE_FFPROBE_BINARY"
        );

        let _ = fs::remove_dir_all(&bin_dir);
    }

    #[test]
    fn selects_arch_specific_macos_ffmpeg_github_urls() {
        let arm64 = macos_ffmpeg_download_urls_for_arch("aarch64");
        assert!(arm64.contains(
            &"https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-arm64"
        ));
        assert!(arm64.contains(
            &"https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-arm64"
        ));
        assert!(!arm64.iter().any(|u| u.contains("evermeet.cx")));

        let x64 = macos_ffmpeg_download_urls_for_arch("x86_64");
        assert!(x64.contains(
            &"https://github.com/eugeneware/ffmpeg-static/releases/latest/download/ffmpeg-darwin-x64"
        ));
        assert!(x64.contains(
            &"https://github.com/eugeneware/ffmpeg-static/releases/latest/download/darwin-x64"
        ));
        assert!(!x64.iter().any(|u| u.contains("evermeet.cx")));
    }

    #[test]
    fn builds_afconvert_wav_output_path() {
        let m4a = PathBuf::from("/tmp/stage/Mr. Brightside [dQw4w9WgXcQ].m4a");
        assert_eq!(
            build_afconvert_wav_path(&m4a),
            PathBuf::from("/tmp/stage/Mr. Brightside [dQw4w9WgXcQ].wav")
        );
    }

    #[test]
    fn detects_supported_video_extensions_case_insensitively() {
        for ext in ["mp4", "MP4", "mov", "MOV", "mkv", "MKV", "webm", "WebM", "avi", "AVI"] {
            assert!(is_video_file(std::path::Path::new(&format!("clip.{ext}"))));
        }
        assert!(!is_video_file(std::path::Path::new("sound.mp3")));
        assert!(!is_video_file(std::path::Path::new("sound.wav")));
        assert!(!is_video_file(std::path::Path::new("cover.png")));
        assert!(!is_video_file(std::path::Path::new("no_ext")));
    }

    #[test]
    fn builds_expected_ffmpeg_mp3_extraction_args() {
        let args = build_ffmpeg_extract_mp3_args(
            std::path::Path::new("C:/clips/intro.mp4"),
            std::path::Path::new("C:/sounds/intro_1234abcd.mp3"),
        );
        let strs: Vec<_> = args.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(
            strs,
            vec![
                "-y",
                "-i",
                "C:/clips/intro.mp4",
                "-vn",
                "-acodec",
                "libmp3lame",
                "-q:a",
                "2",
                "C:/sounds/intro_1234abcd.mp3",
            ]
        );
    }
}
