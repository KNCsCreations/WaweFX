#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod audio;
pub mod config;
pub mod hotkeys;
pub mod sync;
pub mod ui;

use std::sync::Arc;

use eframe::egui;

fn main() -> eframe::Result {
    let cfg = config::AppConfig::load_or_default();
    let audio_engine = audio::AudioEngine::new(&cfg.audio);
    let sync_handle = sync::SyncServerHandle::start(&cfg, audio_engine.clone());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("WaweFX")
        .with_inner_size([840.0, 470.0])
        .with_min_inner_size([720.0, 360.0]);

    #[cfg(not(target_os = "macos"))]
    {
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
