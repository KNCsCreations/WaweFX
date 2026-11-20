#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod audio;
pub mod config;
pub mod hotkeys;
pub mod sync;
pub mod ui;
pub mod updater;

use std::path::Path;
use std::sync::Arc;

use eframe::egui;

fn should_set_viewport_icon_for_exe(exe_path: Option<&Path>) -> bool {
    #[cfg(target_os = "macos")]
    {
        let in_app_bundle = exe_path.is_some_and(|path| {
            let normalized = path.to_string_lossy().replace('\\', "/");
            normalized.contains(".app/Contents/MacOS/")
        });
        !in_app_bundle
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = exe_path;
        true
    }
}

fn main() -> eframe::Result {
    let cfg = config::AppConfig::load_or_default();
    let audio_engine = audio::AudioEngine::new(&cfg.audio);
    audio_engine.set_allow_overlap(cfg.allow_overlap);
    let sync_handle = sync::SyncServerHandle::start(&cfg, audio_engine.clone());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("WaweFX")
        .with_inner_size([840.0, 470.0])
        .with_min_inner_size([720.0, 360.0]);

    let current_exe = std::env::current_exe().ok();
    if should_set_viewport_icon_for_exe(current_exe.as_deref()) {
        viewport = viewport.with_icon(Arc::new(ui::build_window_icon()));
    }

    let native_options = eframe::NativeOptions {
        viewport,
        vsync: false,
        hardware_acceleration: eframe::HardwareAcceleration::Preferred,
        ..Default::default()
    };

    eframe::run_native(
        "WaweFX",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(ui::WaweFxApp::new(
                &cc.egui_ctx,
                cfg,
                audio_engine,
                sync_handle,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::should_set_viewport_icon_for_exe;

    #[test]
    fn viewport_icon_detection_handles_bundle_and_cli_paths() {
        let bundled = Path::new("/Applications/WaweFX.app/Contents/MacOS/WaweFX");
        let cli = Path::new("/Users/dev/WaweFX/target/debug/WaweFX");

        #[cfg(target_os = "macos")]
        {
            assert!(!should_set_viewport_icon_for_exe(Some(bundled)));
            assert!(should_set_viewport_icon_for_exe(Some(cli)));
            assert!(should_set_viewport_icon_for_exe(None));
        }

        #[cfg(not(target_os = "macos"))]
        {
            assert!(should_set_viewport_icon_for_exe(Some(bundled)));
            assert!(should_set_viewport_icon_for_exe(Some(cli)));
            assert!(should_set_viewport_icon_for_exe(None));
        }
    }
}
