use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;
use serde::Deserialize;

/// GitHub `owner/repo` slug detected from the WaweFX origin repository (`KNCsCreations/WaweFX`).
pub const GITHUB_REPO_SLUG: &str = "KNCsCreations/WaweFX";

/// GitHub REST API endpoint queried once in the background at startup.
pub const GITHUB_LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/KNCsCreations/WaweFX/releases/latest";

/// Compiled crate version (`Cargo.toml`).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const UPDATER_USER_AGENT: &str = "WaweFX-Updater";

/// Details about a newer GitHub Release discovered in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub release_notes: String,
}

/// Live state of the non-blocking GitHub Release check shown in Settings and Toolbar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateCheckState {
    #[default]
    Checking,
    UpToDate,
    UpdateAvailable(UpdateInfo),
    OfflineOrUnavailable,
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseResponse {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: String,
}

/// Thread-safe handle to the background GitHub Release update checker.
#[derive(Clone)]
pub struct UpdateChecker {
    latest: Arc<Mutex<Option<UpdateInfo>>>,
    state: Arc<Mutex<UpdateCheckState>>,
}

impl UpdateChecker {
    /// Spawns a detached background thread that checks GitHub Releases once with a 5s timeout.
    pub fn spawn(ctx: egui::Context) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(UpdateCheckState::Checking));

        let latest_bg = Arc::clone(&latest);
        let state_bg = Arc::clone(&state);

        let _ = thread::Builder::new()
            .name("wawefx-update-checker".to_string())
            .spawn(move || {
                match fetch_latest_release() {
                    Ok(Some(info)) => {
                        if let Ok(mut guard) = latest_bg.lock() {
                            *guard = Some(info.clone());
                        }
                        if let Ok(mut st) = state_bg.lock() {
                            *st = UpdateCheckState::UpdateAvailable(info);
                        }
                        ctx.request_repaint();
                    }
                    Ok(None) => {
                        if let Ok(mut st) = state_bg.lock() {
                            *st = UpdateCheckState::UpToDate;
                        }
                    }
                    Err(()) => {
                        // Fail silently when offline, rate-limited, or blocked.
                        if let Ok(mut st) = state_bg.lock() {
                            *st = UpdateCheckState::OfflineOrUnavailable;
                        }
                    }
                }
            });

        Self { latest, state }
    }

    /// Returns `Some(UpdateInfo)` if a strictly newer GitHub Release is available.
    pub fn available_update(&self) -> Option<UpdateInfo> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }

    /// Returns the current update checker state for display in Settings.
    pub fn check_state(&self) -> UpdateCheckState {
        self.state
            .lock()
            .map(|g| g.clone())
            .unwrap_or(UpdateCheckState::OfflineOrUnavailable)
    }
}

fn fetch_latest_release() -> Result<Option<UpdateInfo>, ()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    let response = agent
        .get(GITHUB_LATEST_RELEASE_API_URL)
        .set("User-Agent", UPDATER_USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|_| ())?;

    if response.status() != 200 {
        return Err(());
    }

    let body = response.into_string().map_err(|_| ())?;
    evaluate_release_json(CURRENT_VERSION, &body)
}

/// Parses a GitHub Releases JSON payload and returns `Some(UpdateInfo)` when `tag_name` is
/// strictly newer than `current_version`.
pub fn evaluate_release_json(current_version: &str, body: &str) -> Result<Option<UpdateInfo>, ()> {
    let parsed: GitHubReleaseResponse = serde_json::from_str(body).map_err(|_| ())?;
    let trimmed_url = parsed.html_url.trim();
    if !trimmed_url.starts_with("https://") && !trimmed_url.starts_with("http://") {
        return Err(());
    }

    if !is_version_newer(current_version, &parsed.tag_name) {
        return Ok(None);
    }

    let clean_ver = parsed
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();

    Ok(Some(UpdateInfo {
        version: clean_ver,
        url: trimmed_url.to_string(),
        release_notes: parsed.body,
    }))
}

/// Parses `"v1.2.3"` / `"1.2.3-beta"` into `(major, minor, patch)`.
pub fn parse_semver_triplet(raw: &str) -> Option<(u64, u64, u64)> {
    let stripped = raw.trim().trim_start_matches(['v', 'V']);
    let core = stripped.split(['-', '+']).next()?.trim();
    if core.is_empty() {
        return None;
    }

    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts
        .next()
        .map(str::parse::<u64>)
        .transpose()
        .ok()?
        .unwrap_or(0);
    let patch = parts
        .next()
        .map(str::parse::<u64>)
        .transpose()
        .ok()?
        .unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Returns `true` strictly when `remote_tag` represents a higher `(major, minor, patch)` version
/// than `current`.
pub fn is_version_newer(current: &str, remote_tag: &str) -> bool {
    match (parse_semver_triplet(current), parse_semver_triplet(remote_tag)) {
        (Some(cur), Some(rem)) => rem > cur,
        _ => false,
    }
}

/// Opens a GitHub Release URL in the user's default browser without blocking the UI thread.
pub fn open_release_url(url: &str) {
    let trimmed = url.trim();
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return;
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", trimmed])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(trimmed).spawn();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(trimmed).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::{evaluate_release_json, is_version_newer, parse_semver_triplet};

    #[test]
    fn parses_and_compares_semver_versions() {
        assert_eq!(parse_semver_triplet("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver_triplet("v1.2.10"), Some((1, 2, 10)));
        assert_eq!(parse_semver_triplet("V2.0"), Some((2, 0, 0)));
        assert_eq!(parse_semver_triplet("v1.3.0-rc.1"), Some((1, 3, 0)));
        assert_eq!(parse_semver_triplet("invalid"), None);

        assert!(is_version_newer("0.1.0", "v0.1.1"));
        assert!(is_version_newer("0.1.0", "v1.0.0"));
        assert!(is_version_newer("1.2.3", "1.10.0"));
        assert!(!is_version_newer("0.1.0", "v0.1.0"));
        assert!(!is_version_newer("1.2.0", "v1.1.9"));
    }

    #[test]
    fn evaluates_github_release_json_payloads() {
        let newer_json = r#"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/KNCsCreations/WaweFX/releases/tag/v0.2.0",
            "body": "Bug fixes and performance improvements."
        }"#;
        let info = evaluate_release_json("0.1.0", newer_json)
            .expect("valid json")
            .expect("0.2.0 > 0.1.0");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(
            info.url,
            "https://github.com/KNCsCreations/WaweFX/releases/tag/v0.2.0"
        );
        assert_eq!(info.release_notes, "Bug fixes and performance improvements.");

        assert_eq!(evaluate_release_json("0.2.0", newer_json), Ok(None));
        assert_eq!(evaluate_release_json("0.3.0", newer_json), Ok(None));
        assert!(evaluate_release_json("0.1.0", "not json").is_err());
    }
}
