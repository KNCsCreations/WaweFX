use std::f32::consts::{FRAC_PI_2, PI};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::audio::AudioEngine;
use crate::config::{
    AppConfig, MAX_PLAYBACK_SPEED, MAX_STREAM_VOLUME, MIN_PLAYBACK_SPEED, MIN_STREAM_VOLUME,
    rgb_to_color32,
};

/// Height of the top toolbar strip above the 1px divider line.
pub const TOOLBAR_HEIGHT: f32 = 46.0;

/// Events emitted from toolbar interactions on a single frame.
#[derive(Debug, Default)]
pub struct ToolbarAction {
    pub add_sound: bool,
    pub remove_selected: bool,
    pub edit_selected: bool,
    pub play_selected: bool,
    pub stop_playback: bool,
    pub rewind: bool,
    pub fast_forward: bool,
    pub seeked: bool,
    pub open_alerts: bool,
    pub open_companion: bool,
    pub open_settings: bool,
    pub config_dirty: bool,
}

/// Renders the pixel-accurate WaweFX top toolbar matching the reference mockup.
pub fn render_toolbar(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    audio: &AudioEngine,
    has_audio_warning: bool,
) -> ToolbarAction {
    let mut action = ToolbarAction::default();
    let theme = &config.theme;

    let bg_toolbar = rgb_to_color32(theme.bg_toolbar);
    let divider_color = rgb_to_color32(theme.toolbar_divider);
    let shadow_color = rgb_to_color32(theme.toolbar_shadow);
    let icon_primary = rgb_to_color32(theme.icon_primary);
    let icon_secondary = rgb_to_color32(theme.icon_secondary);
    let icon_phone = rgb_to_color32(theme.icon_phone);
    let gauge_color = rgb_to_color32(theme.gauge_and_track);
    let accent = rgb_to_color32(theme.accent_color);
    let text_color = rgb_to_color32(theme.pad_text);

    let avail = ui.available_rect_before_wrap();
    let bar_rect = Rect::from_min_size(avail.min, Vec2::new(avail.width().max(640.0), TOOLBAR_HEIGHT));
    ui.allocate_rect(bar_rect, Sense::hover());

    let x0 = bar_rect.min.x;
    let y0 = bar_rect.min.y;
    let has_selection = config.selected_sound_id.is_some();
    let is_playing = audio.is_playing();

    // --- Left Group: Soundboard CRUD (`+`, `-`, `...`) ---
    let add_rect = Rect::from_min_size(Pos2::new(x0 + 10.0, y0 + 10.0), Vec2::splat(25.0));
    if paint_circle_icon_button(ui, "tb_add", add_rect, icon_primary, bg_toolbar, "Add Sound (+)", |p, c, bg| {
        p.circle_filled(c, 12.5, icon_primary);
        p.line_segment([Pos2::new(c.x - 5.5, c.y), Pos2::new(c.x + 5.5, c.y)], Stroke::new(2.6_f32, bg));
        p.line_segment([Pos2::new(c.x, c.y - 5.5), Pos2::new(c.x, c.y + 5.5)], Stroke::new(2.6_f32, bg));
    }) {
        action.add_sound = true;
    }

    let remove_rect = Rect::from_min_size(Pos2::new(x0 + 41.0, y0 + 10.0), Vec2::splat(25.0));
    let remove_col = if has_selection { icon_primary } else { dim_color(icon_primary, 0.55) };
    if paint_circle_icon_button(
        ui,
        "tb_remove",
        remove_rect,
        remove_col,
        bg_toolbar,
        "Remove Selected Sound (-)",
        |p, c, bg| {
            p.circle_filled(c, 12.5, remove_col);
            p.line_segment([Pos2::new(c.x - 5.5, c.y), Pos2::new(c.x + 5.5, c.y)], Stroke::new(2.8_f32, bg));
        },
    ) && has_selection
    {
        action.remove_selected = true;
    }

    let edit_rect = Rect::from_min_size(Pos2::new(x0 + 72.0, y0 + 10.0), Vec2::splat(25.0));
    let edit_col = if has_selection { icon_primary } else { dim_color(icon_primary, 0.55) };
    if paint_circle_icon_button(
        ui,
        "tb_edit",
        edit_rect,
        edit_col,
        bg_toolbar,
        "Edit Sound Properties (...)",
        |p, c, bg| {
            p.circle_filled(c, 12.5, edit_col);
            for dx in [-5.2f32, 0.0, 5.2] {
                p.circle_filled(Pos2::new(c.x + dx, c.y), 1.8, bg);
            }
        },
    ) && has_selection
    {
        action.edit_selected = true;
    }

    // --- Transport Controls (`Play`, `Stop`, `Rewind`, `FastForward`) ---
    let play_rect = Rect::from_min_size(Pos2::new(x0 + 118.0, y0 + 10.0), Vec2::splat(25.0));
    let play_fill = if is_playing { accent } else { icon_primary };
    if paint_circle_icon_button(ui, "tb_play", play_rect, play_fill, bg_toolbar, "Play Selected Sound", |p, c, bg| {
        p.circle_filled(c, 12.5, play_fill);
        let pts = vec![
            Pos2::new(c.x - 3.6, c.y - 5.4),
            Pos2::new(c.x + 6.0, c.y),
            Pos2::new(c.x - 3.6, c.y + 5.4),
        ];
        p.add(egui::Shape::convex_polygon(pts, bg, Stroke::NONE));
    }) {
        action.play_selected = true;
    }

    let stop_rect = Rect::from_min_size(Pos2::new(x0 + 149.0, y0 + 10.0), Vec2::splat(25.0));
    if paint_circle_icon_button(ui, "tb_stop", stop_rect, icon_primary, bg_toolbar, "Stop Playback", |p, c, bg| {
        p.circle_filled(c, 12.5, icon_primary);
        let sq = Rect::from_center_size(c, Vec2::splat(9.0));
        p.rect_filled(sq, 1.2, bg);
    }) {
        action.stop_playback = true;
    }

    let rw_rect = Rect::from_min_size(Pos2::new(x0 + 180.0, y0 + 14.0), Vec2::new(25.0, 17.0));
    if paint_double_arrow_button(ui, "tb_rw", rw_rect, icon_secondary, true, "Rewind (-5s)") {
        action.rewind = true;
    }

    let ff_rect = Rect::from_min_size(Pos2::new(x0 + 211.0, y0 + 14.0), Vec2::new(25.0, 17.0));
    if paint_double_arrow_button(ui, "tb_ff", ff_rect, icon_secondary, false, "Fast Forward (+5s)") {
        action.fast_forward = true;
    }

    // --- Right Cluster Anchor ---
    let right_base = (bar_rect.max.x - 240.0).max(x0 + 280.0);

    // --- Center Horizontal Timeline / Seek Bar ---
    let slider_left = (x0 + 275.0).min(right_base - 95.0);
    let slider_right = (right_base - 35.0).max(slider_left + 60.0);
    let slider_rect = Rect::from_min_max(
        Pos2::new(slider_left, y0 + 15.0),
        Pos2::new(slider_right, y0 + 31.0),
    );
    if paint_timeline_seek_bar(
        ui,
        slider_rect,
        audio,
        config,
        gauge_color,
        accent,
        text_color,
    ) {
        action.seeked = true;
    }

    // --- Left Volume Gauge Knob (Mic Stream Volume) ---
    let left_knob_rect = Rect::from_min_size(Pos2::new(right_base, y0 + 11.0), Vec2::new(24.0, 21.0));
    if paint_rotary_knob(
        ui,
        "tb_mic_knob",
        left_knob_rect,
        &mut config.audio.mic_volume,
        gauge_color,
        accent,
        "Mic Stream Volume",
    ) {
        audio.set_mic_volume(config.audio.mic_volume);
        action.config_dirty = true;
    }

    // --- Microphone Mute/Unmute Toggle ---
    let mic_rect = Rect::from_min_size(Pos2::new(right_base + 34.0, y0 + 10.0), Vec2::new(19.0, 25.0));
    if paint_mic_toggle(
        ui,
        mic_rect,
        config.audio.mic_stream_enabled,
        icon_primary,
        accent,
    ) {
        config.audio.mic_stream_enabled = !config.audio.mic_stream_enabled;
        audio.set_mic_enabled(config.audio.mic_stream_enabled);
        action.config_dirty = true;
    }

    // --- Speaker Mute/Unmute Toggle ---
    let speaker_rect = Rect::from_min_size(Pos2::new(right_base + 62.0, y0 + 12.0), Vec2::new(25.0, 20.0));
    if paint_speaker_toggle(
        ui,
        speaker_rect,
        config.audio.headphones_stream_enabled,
        icon_primary,
        accent,
    ) {
        config.audio.headphones_stream_enabled = !config.audio.headphones_stream_enabled;
        audio.set_headphones_enabled(config.audio.headphones_stream_enabled);
        action.config_dirty = true;
    }

    // --- Right Volume Gauge Knob (Headphones Monitoring Volume) ---
    let right_knob_rect =
        Rect::from_min_size(Pos2::new(right_base + 96.0, y0 + 11.0), Vec2::new(24.0, 21.0));
    if paint_rotary_knob(
        ui,
        "tb_hp_knob",
        right_knob_rect,
        &mut config.audio.headphones_volume,
        gauge_color,
        accent,
        "Headphones Monitoring Volume",
    ) {
        audio.set_headphones_volume(config.audio.headphones_volume);
        action.config_dirty = true;
    }

    // --- Right System Icons (`!`, `Phone`, `Gear`) ---
    let alert_rect =
        Rect::from_min_size(Pos2::new(right_base + 139.0, y0 + 10.0), Vec2::splat(25.0));
    if paint_alert_button(ui, alert_rect, icon_primary, bg_toolbar, accent, has_audio_warning) {
        action.open_alerts = true;
    }

    let phone_rect =
        Rect::from_min_size(Pos2::new(right_base + 174.0, y0 + 10.0), Vec2::new(16.0, 25.0));
    if paint_phone_button(ui, phone_rect, icon_phone, bg_toolbar) {
        action.open_companion = true;
    }

    let gear_rect =
        Rect::from_min_size(Pos2::new(right_base + 201.0, y0 + 10.0), Vec2::splat(25.0));
    if paint_gear_button(ui, gear_rect, icon_primary, bg_toolbar) {
        action.open_settings = true;
    }

    // --- Bottom 1px Divider + 1px Shadow ---
    let painter = ui.painter();
    let y_div = bar_rect.max.y - 1.5;
    painter.line_segment(
        [Pos2::new(bar_rect.min.x, y_div), Pos2::new(bar_rect.max.x, y_div)],
        Stroke::new(1.0_f32, divider_color),
    );
    painter.line_segment(
        [
            Pos2::new(bar_rect.min.x, y_div + 1.0),
            Pos2::new(bar_rect.max.x, y_div + 1.0),
        ],
        Stroke::new(1.0_f32, shadow_color),
    );

    action
}

fn paint_circle_icon_button(
    ui: &mut egui::Ui,
    id: &str,
    rect: Rect,
    _fill: Color32,
    cutout: Color32,
    tooltip: &str,
    draw_fn: impl FnOnce(&egui::Painter, Pos2, Color32),
) -> bool {
    let resp = ui.interact(rect, ui.id().with(id), Sense::click());
    let center = rect.center();
    let painter = ui.painter();

    if resp.hovered() {
        painter.circle_filled(center, 14.5, Color32::from_black_alpha(22));
    }
    draw_fn(painter, center, cutout);
    resp.on_hover_text(tooltip).clicked()
}

fn paint_double_arrow_button(
    ui: &mut egui::Ui,
    id: &str,
    rect: Rect,
    color: Color32,
    left_pointing: bool,
    tooltip: &str,
) -> bool {
    let resp = ui.interact(rect, ui.id().with(id), Sense::click());
    let painter = ui.painter();
    let tint = if resp.hovered() {
        brighten_color(color, 18)
    } else {
        color
    };

    let cy = rect.center().y;
    let half_h = 8.0;
    let tri_w = 11.5;

    if left_pointing {
        for offset in [0.0f32, 11.5] {
            let tip_x = rect.min.x + 1.0 + offset;
            let pts = vec![
                Pos2::new(tip_x, cy),
                Pos2::new(tip_x + tri_w, cy - half_h),
                Pos2::new(tip_x + tri_w, cy + half_h),
            ];
            painter.add(egui::Shape::convex_polygon(pts, tint, Stroke::NONE));
        }
    } else {
        for offset in [0.0f32, 11.5] {
            let base_x = rect.min.x + 1.0 + offset;
            let pts = vec![
                Pos2::new(base_x, cy - half_h),
                Pos2::new(base_x + tri_w, cy),
                Pos2::new(base_x, cy + half_h),
            ];
            painter.add(egui::Shape::convex_polygon(pts, tint, Stroke::NONE));
        }
    }

    resp.on_hover_text(tooltip).clicked()
}

fn format_mm_ss(secs: f32) -> String {
    let total = secs.max(0.0).round() as u32;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn paint_timeline_seek_bar(
    ui: &mut egui::Ui,
    rect: Rect,
    audio: &AudioEngine,
    config: &AppConfig,
    track_color: Color32,
    thumb_color: Color32,
    text_color: Color32,
) -> bool {
    let mut seeked = false;
    let is_playing = audio.is_playing();
    let total_secs = if is_playing {
        audio.active_sound_duration_secs()
    } else if let Some(sel_id) = config.selected_sound_id.as_deref() {
        config
            .sounds
            .iter()
            .find(|s| s.id == sel_id)
            .map(|s| s.duration_secs)
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let resp = ui.interact(
        rect.expand(4.0),
        ui.id().with("tb_timeline_seek"),
        Sense::click_and_drag(),
    );

    let thumb_r = 8.0;
    let track_min_x = rect.min.x + thumb_r;
    let track_max_x = (rect.max.x - thumb_r).max(track_min_x + 1.0);
    let cy = rect.center().y;

    #[derive(Clone, Copy, Default)]
    struct TimelineScrubState {
        active: bool,
        was_playing: bool,
        target_progress: f32,
    }

    let scrub_key = egui::Id::new("tb_timeline_scrub_state");
    let mut scrub: TimelineScrubState = ui.data(|d| d.get_temp(scrub_key)).unwrap_or_default();
    let pointer_to_progress = |px: f32| ((px - track_min_x) / (track_max_x - track_min_x)).clamp(0.0, 1.0);

    let live_progress = if is_playing {
        audio.playback_progress().clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut progress = live_progress;

    if resp.drag_started() {
        let t = resp
            .interact_pointer_pos()
            .map(|p| pointer_to_progress(p.x))
            .unwrap_or(live_progress);
        scrub = TimelineScrubState {
            active: true,
            was_playing: is_playing,
            target_progress: t,
        };
        if is_playing {
            audio.set_scrub_paused(true);
            progress = t;
        }
    } else if scrub.active && resp.dragged() {
        if let Some(pointer) = resp.interact_pointer_pos() {
            scrub.target_progress = pointer_to_progress(pointer.x);
        }
        if scrub.was_playing {
            progress = scrub.target_progress;
        }
    } else if scrub.active && (resp.drag_stopped() || !resp.dragged()) {
        if let Some(pointer) = resp.interact_pointer_pos() {
            scrub.target_progress = pointer_to_progress(pointer.x);
        }
        if scrub.was_playing && audio.is_playing() {
            audio.seek_to_progress(scrub.target_progress);
            progress = scrub.target_progress;
            seeked = true;
        }
        audio.set_scrub_paused(false);
        scrub = TimelineScrubState::default();
    } else if resp.clicked()
        && is_playing
        && let Some(pointer) = resp.interact_pointer_pos()
    {
        let t = pointer_to_progress(pointer.x);
        audio.seek_to_progress(t);
        progress = t;
        seeked = true;
    }

    ui.data_mut(|d| d.insert_temp(scrub_key, scrub));

    let thumb_x = track_min_x + progress * (track_max_x - track_min_x);
    let thumb_center = Pos2::new(thumb_x, cy);

    let painter = ui.painter();
    painter.line_segment(
        [Pos2::new(rect.min.x, cy), Pos2::new(rect.max.x, cy)],
        Stroke::new(2.4_f32, track_color),
    );
    if is_playing && thumb_x > rect.min.x {
        painter.line_segment(
            [Pos2::new(rect.min.x, cy), Pos2::new(thumb_x, cy)],
            Stroke::new(2.8_f32, thumb_color),
        );
    }
    if resp.hovered() || resp.dragged() {
        painter.circle_filled(
            thumb_center,
            thumb_r + 3.0,
            Color32::from_rgba_unmultiplied(
                thumb_color.r(),
                thumb_color.g(),
                thumb_color.b(),
                60,
            ),
        );
    }
    painter.circle_filled(thumb_center, thumb_r, thumb_color);

    let elapsed_secs = total_secs * progress;
    let time_text = format!("{} / {}", format_mm_ss(elapsed_secs), format_mm_ss(total_secs));

    if is_playing || resp.hovered() || !config.sounds.is_empty() {
        painter.text(
            Pos2::new(rect.center().x, rect.max.y + 7.5),
            egui::Align2::CENTER_CENTER,
            &time_text,
            egui::FontId::proportional(9.5),
            dim_color(text_color, 0.8),
        );
    }

    let tip = if is_playing {
        format!("Timeline: {time_text} (Click or drag to seek)")
    } else {
        format!("Timeline: {time_text} (Play a sound to scrub)")
    };
    resp.on_hover_text(tip);
    seeked
}

/// Renders the vertical Pitch / Speed slider docked on the right side of the main area.
pub fn render_vertical_pitch_panel(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    audio: &AudioEngine,
) -> bool {
    let mut changed = false;
    let track_color = rgb_to_color32(config.theme.gauge_and_track);
    let accent = rgb_to_color32(config.theme.accent_color);
    let text_color = rgb_to_color32(config.theme.pad_text);

    let panel_rect = ui.available_rect_before_wrap();
    ui.allocate_rect(panel_rect, Sense::hover());

    let cx = panel_rect.center().x;
    let top_label_y = panel_rect.min.y + 16.0;
    let bottom_label_y = panel_rect.max.y - 18.0;

    let slider_top = top_label_y + 18.0;
    let slider_bottom = (bottom_label_y - 18.0).max(slider_top + 60.0);
    let track_rect = Rect::from_min_max(
        Pos2::new(cx - 12.0, slider_top),
        Pos2::new(cx + 12.0, slider_bottom),
    );

    let resp = ui.interact(
        track_rect.expand(4.0),
        ui.id().with("right_vertical_pitch_slider"),
        Sense::click_and_drag(),
    );

    let thumb_r = 7.5;
    let min_y = slider_top + thumb_r;
    let max_y = (slider_bottom - thumb_r).max(min_y + 1.0);

    if resp.double_clicked() || resp.secondary_clicked() {
        config.audio.playback_speed = 1.0;
        changed = true;
    } else if (resp.dragged() || resp.clicked())
        && let Some(pointer) = resp.interact_pointer_pos()
    {
        // Bottom = 0.5x (norm 0.0), Top = 2.0x (norm 1.0)
        let t = ((max_y - pointer.y) / (max_y - min_y)).clamp(0.0, 1.0);
        config.audio.playback_speed =
            MIN_PLAYBACK_SPEED + t * (MAX_PLAYBACK_SPEED - MIN_PLAYBACK_SPEED);
        changed = true;
    }

    if resp.hovered() {
        let scroll_y = ui.input(|i| i.raw_scroll_delta.y);
        if scroll_y.abs() > 0.0 {
            config.audio.playback_speed = (config.audio.playback_speed + scroll_y * 0.0015)
                .clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
            changed = true;
        }
    }

    if changed {
        audio.set_speed(config.audio.playback_speed);
    }

    let speed = config
        .audio
        .playback_speed
        .clamp(MIN_PLAYBACK_SPEED, MAX_PLAYBACK_SPEED);
    let norm = ((speed - MIN_PLAYBACK_SPEED) / (MAX_PLAYBACK_SPEED - MIN_PLAYBACK_SPEED))
        .clamp(0.0, 1.0);
    let thumb_y = max_y - norm * (max_y - min_y);
    let thumb_center = Pos2::new(cx, thumb_y);

    let default_norm = (1.0 - MIN_PLAYBACK_SPEED) / (MAX_PLAYBACK_SPEED - MIN_PLAYBACK_SPEED);
    let tick_y = max_y - default_norm * (max_y - min_y);

    let painter = ui.painter();
    painter.text(
        Pos2::new(cx, top_label_y),
        egui::Align2::CENTER_CENTER,
        "PITCH",
        egui::FontId::proportional(9.5),
        dim_color(text_color, 0.78),
    );

    // Subtle vertical separator on left edge of pitch panel
    painter.line_segment(
        [
            Pos2::new(panel_rect.min.x + 0.5, panel_rect.min.y + 8.0),
            Pos2::new(panel_rect.min.x + 0.5, panel_rect.max.y - 8.0),
        ],
        Stroke::new(1.0_f32, Color32::from_black_alpha(26)),
    );

    // 1.0x reference tick mark
    painter.line_segment(
        [Pos2::new(cx - 8.0, tick_y), Pos2::new(cx + 8.0, tick_y)],
        Stroke::new(1.4_f32, dim_color(track_color, 0.7)),
    );

    // Vertical track
    painter.line_segment(
        [Pos2::new(cx, min_y), Pos2::new(cx, max_y)],
        Stroke::new(3.0_f32, track_color),
    );

    // Accent highlight from 1.0x mark to current thumb position
    painter.line_segment(
        [Pos2::new(cx, tick_y), Pos2::new(cx, thumb_y)],
        Stroke::new(3.2_f32, accent),
    );

    if resp.hovered() || resp.dragged() {
        painter.circle_filled(
            thumb_center,
            thumb_r + 3.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 65),
        );
    }
    painter.circle_filled(thumb_center, thumb_r, accent);

    // Speed label (clickable to reset 1.0x)
    let label_rect = Rect::from_center_size(Pos2::new(cx, bottom_label_y), Vec2::new(40.0, 18.0));
    let label_resp = ui.interact(
        label_rect,
        ui.id().with("right_pitch_reset"),
        Sense::click(),
    );
    if label_resp.clicked() {
        config.audio.playback_speed = 1.0;
        audio.set_speed(1.0);
        changed = true;
    }
    painter.text(
        label_rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{:.2}x", config.audio.playback_speed),
        egui::FontId::proportional(10.5),
        if (config.audio.playback_speed - 1.0).abs() > 0.01 {
            accent
        } else {
            text_color
        },
    );
    label_resp.on_hover_text("Click to reset Pitch to 1.00x");
    resp.on_hover_text(format!(
        "Pitch / Speed: {:.2}x (Drag, scroll, or double-click to reset 1.00x)",
        config.audio.playback_speed
    ));

    changed
}

fn paint_rotary_knob(
    ui: &mut egui::Ui,
    id: &str,
    rect: Rect,
    volume: &mut f32,
    gauge_color: Color32,
    accent: Color32,
    label: &str,
) -> bool {
    let mut changed = false;
    let resp = ui.interact(rect.expand(4.0), ui.id().with(id), Sense::click_and_drag());

    if resp.double_clicked() || resp.secondary_clicked() {
        *volume = 1.0;
        changed = true;
    } else if resp.dragged() {
        let delta = -resp.drag_delta().y * 0.01;
        if delta.abs() > f32::EPSILON {
            *volume = (*volume + delta).clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
            changed = true;
        }
    }

    if resp.hovered() {
        let scroll_y = ui.input(|i| i.raw_scroll_delta.y);
        if scroll_y.abs() > 0.0 {
            *volume = (*volume + scroll_y * 0.002).clamp(MIN_STREAM_VOLUME, MAX_STREAM_VOLUME);
            changed = true;
        }
    }

    let center = Pos2::new(rect.center().x, rect.center().y + 1.5);
    let radius = 10.0;
    let start_angle = -5.0 * PI / 4.0; // -225 deg (bottom-left)
    let sweep = 1.5 * PI; // 270 deg across top (-90 deg = 100%) to +45 deg (bottom-right)
    let segments = 24usize;

    let arc_pts: Vec<Pos2> = (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let theta = start_angle + t * sweep;
            Pos2::new(center.x + radius * theta.cos(), center.y + radius * theta.sin())
        })
        .collect();

    let painter = ui.painter();
    let ring_color = if resp.hovered() || resp.dragged() {
        accent
    } else {
        gauge_color
    };
    painter.add(egui::Shape::line(arc_pts, Stroke::new(2.2_f32, ring_color)));

    // Indicator needle pointing straight up (-90 deg) at 1.0 (100%).
    let norm = ((*volume - MIN_STREAM_VOLUME) / (MAX_STREAM_VOLUME - MIN_STREAM_VOLUME)).clamp(0.0, 1.0);
    let needle_theta = start_angle + norm * sweep;
    let needle_tip = Pos2::new(
        center.x + (radius - 1.5) * needle_theta.cos(),
        center.y + (radius - 1.5) * needle_theta.sin(),
    );
    painter.line_segment([center, needle_tip], Stroke::new(2.2_f32, ring_color));
    painter.circle_filled(center, 1.8, ring_color);

    let pct = (*volume * 100.0).round() as u32;
    resp.on_hover_text(format!("{label}: {pct}% (Drag up/down or scroll, double-click = 100%)"));
    changed
}

fn paint_mic_toggle(
    ui: &mut egui::Ui,
    rect: Rect,
    enabled: bool,
    primary: Color32,
    accent: Color32,
) -> bool {
    let resp = ui.interact(rect.expand(3.0), ui.id().with("tb_mic_toggle"), Sense::click());
    let painter = ui.painter();
    let cx = rect.center().x;

    if enabled {
        painter.circle_filled(
            rect.center(),
            13.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if resp.hovered() { 65 } else { 32 }),
        );
    } else if resp.hovered() {
        painter.circle_filled(rect.center(), 13.0, Color32::from_black_alpha(20));
    }

    let fg = if enabled {
        primary
    } else {
        dim_color(primary, 0.45)
    };

    // Capsule body
    let capsule = Rect::from_center_size(Pos2::new(cx, rect.min.y + 7.2), Vec2::new(7.4, 13.0));
    painter.rect_filled(capsule, 3.7, fg);

    // U-shaped cradle around bottom of capsule
    let cradle_center = Pos2::new(cx, rect.min.y + 9.2);
    let cradle_r = 6.5;
    let mut cradle_pts = Vec::with_capacity(15);
    cradle_pts.push(Pos2::new(cradle_center.x - cradle_r, cradle_center.y - 2.2));
    for i in 0..=12 {
        let theta = PI - (i as f32 / 12.0) * PI;
        cradle_pts.push(Pos2::new(
            cradle_center.x + cradle_r * theta.cos(),
            cradle_center.y + cradle_r * theta.sin(),
        ));
    }
    cradle_pts.push(Pos2::new(cradle_center.x + cradle_r, cradle_center.y - 2.2));
    painter.add(egui::Shape::line(cradle_pts, Stroke::new(1.9_f32, fg)));

    // Stem + foot base
    let stem_top = cradle_center.y + cradle_r;
    let foot_y = rect.max.y - 1.5;
    painter.line_segment([Pos2::new(cx, stem_top), Pos2::new(cx, foot_y)], Stroke::new(2.0_f32, fg));
    painter.line_segment(
        [Pos2::new(cx - 5.5, foot_y), Pos2::new(cx + 5.5, foot_y)],
        Stroke::new(2.2_f32, fg),
    );

    if !enabled {
        painter.line_segment(
            [
                Pos2::new(rect.min.x - 1.0, rect.max.y - 2.0),
                Pos2::new(rect.max.x + 1.0, rect.min.y + 2.0),
            ],
            Stroke::new(2.2_f32, accent),
        );
    }

    let tip = if enabled {
        "Mic Soundboard Routing: ACTIVE (Click to mute)"
    } else {
        "Mic Soundboard Routing: MUTED (Click to enable)"
    };
    resp.on_hover_text(tip).clicked()
}

fn paint_speaker_toggle(
    ui: &mut egui::Ui,
    rect: Rect,
    enabled: bool,
    primary: Color32,
    accent: Color32,
) -> bool {
    let resp = ui.interact(rect.expand(3.0), ui.id().with("tb_speaker_toggle"), Sense::click());
    let painter = ui.painter();

    if enabled {
        painter.circle_filled(
            rect.center(),
            13.0,
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if resp.hovered() { 65 } else { 32 }),
        );
    } else if resp.hovered() {
        painter.circle_filled(rect.center(), 13.0, Color32::from_black_alpha(20));
    }

    let fg = if enabled {
        primary
    } else {
        dim_color(primary, 0.45)
    };

    let cy = rect.center().y;
    let x = rect.min.x;

    // Speaker back box + flared cone
    let box_rect = Rect::from_min_max(Pos2::new(x + 1.0, cy - 3.6), Pos2::new(x + 6.2, cy + 3.6));
    painter.rect_filled(box_rect, 1.0, fg);
    let cone = vec![
        Pos2::new(x + 5.2, cy - 3.6),
        Pos2::new(x + 12.2, cy - 8.6),
        Pos2::new(x + 12.2, cy + 8.6),
        Pos2::new(x + 5.2, cy + 3.6),
    ];
    painter.add(egui::Shape::convex_polygon(cone, fg, Stroke::NONE));

    // Dual curved sound wave arcs (active only or dimmed when muted)
    let wave_origin = Pos2::new(x + 12.6, cy);
    for (r, sweep) in [(4.6f32, 0.78f32), (8.2f32, 0.86f32)] {
        let pts: Vec<Pos2> = (0..=10)
            .map(|i| {
                let t = i as f32 / 10.0;
                let theta = -sweep + t * (2.0 * sweep);
                Pos2::new(
                    wave_origin.x + r * theta.cos(),
                    wave_origin.y + r * theta.sin(),
                )
            })
            .collect();
        painter.add(egui::Shape::line(pts, Stroke::new(2.0_f32, fg)));
    }

    if !enabled {
        painter.line_segment(
            [
                Pos2::new(rect.min.x, rect.max.y),
                Pos2::new(rect.max.x, rect.min.y),
            ],
            Stroke::new(2.2_f32, accent),
        );
    }

    let tip = if enabled {
        "Headphones Monitoring: ACTIVE (Click to mute)"
    } else {
        "Headphones Monitoring: MUTED (Click to enable)"
    };
    resp.on_hover_text(tip).clicked()
}

fn paint_alert_button(
    ui: &mut egui::Ui,
    rect: Rect,
    primary: Color32,
    cutout: Color32,
    accent: Color32,
    has_warning: bool,
) -> bool {
    let resp = ui.interact(rect, ui.id().with("tb_alert"), Sense::click());
    let painter = ui.painter();
    let center = rect.center();

    if resp.hovered() {
        painter.circle_filled(center, 14.5, Color32::from_black_alpha(22));
    }

    let circle_col = if has_warning { accent } else { primary };
    painter.circle_filled(center, 12.5, circle_col);
    painter.line_segment(
        [Pos2::new(center.x, center.y - 6.0), Pos2::new(center.x, center.y + 1.5)],
        Stroke::new(2.8_f32, cutout),
    );
    painter.circle_filled(Pos2::new(center.x, center.y + 5.5), 1.6, cutout);

    if has_warning {
        let badge_pos = Pos2::new(center.x + 9.5, center.y - 9.5);
        painter.circle_filled(badge_pos, 4.5, cutout);
        painter.circle_filled(badge_pos, 3.0, Color32::from_rgb(230, 55, 55));
    }

    let tip = if has_warning {
        "Audio Engine / Virtual Mic Setup Alert — Click for details"
    } else {
        "Audio Engine Status: Healthy"
    };
    resp.on_hover_text(tip).clicked()
}

fn paint_phone_button(
    ui: &mut egui::Ui,
    rect: Rect,
    phone_color: Color32,
    cutout: Color32,
) -> bool {
    let resp = ui.interact(rect.expand(3.0), ui.id().with("tb_phone"), Sense::click());
    let painter = ui.painter();

    if resp.hovered() {
        painter.rect_filled(rect.expand(3.0), 4.0, Color32::from_black_alpha(22));
    }

    painter.rect_filled(rect, 2.8, phone_color);
    // Subtle top speaker slot & bottom home dot matching smartphone silhouette.
    let speaker = Rect::from_center_size(
        Pos2::new(rect.center().x, rect.min.y + 3.0),
        Vec2::new(5.0, 1.3),
    );
    painter.rect_filled(speaker, 0.6, cutout);
    painter.circle_filled(Pos2::new(rect.center().x, rect.max.y - 2.8), 1.3, cutout);

    resp.on_hover_text("Android / Mobile Companion Sync (QR Code)").clicked()
}

fn paint_gear_button(
    ui: &mut egui::Ui,
    rect: Rect,
    primary: Color32,
    cutout: Color32,
) -> bool {
    let resp = ui.interact(rect, ui.id().with("tb_gear"), Sense::click());
    let painter = ui.painter();
    let c = rect.center();

    if resp.hovered() {
        painter.circle_filled(c, 14.5, Color32::from_black_alpha(22));
    }

    let fg = if resp.hovered() {
        brighten_color(primary, 16)
    } else {
        primary
    };

    // 8 cog teeth + central hub + cutout hole
    for i in 0..8 {
        let angle = (i as f32) * (PI / 4.0) - FRAC_PI_2;
        let dir = Vec2::new(angle.cos(), angle.sin());
        let perp = Vec2::new(-dir.y, dir.x);
        let r0 = 7.6;
        let r1 = 12.2;
        let hw_base = 2.4;
        let hw_tip = 1.9;
        let tooth = vec![
            c + dir * r0 - perp * hw_base,
            c + dir * r1 - perp * hw_tip,
            c + dir * r1 + perp * hw_tip,
            c + dir * r0 + perp * hw_base,
        ];
        painter.add(egui::Shape::convex_polygon(tooth, fg, Stroke::NONE));
    }
    painter.circle_filled(c, 9.2, fg);
    painter.circle_filled(c, 4.1, cutout);
    painter.circle_stroke(c, 4.1, Stroke::new(0.5_f32, fg));

    resp.on_hover_text("Settings (Audio Devices & Color Themes)").clicked()
}

fn dim_color(color: Color32, factor: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * factor).clamp(0.0, 255.0) as u8,
    )
}

fn brighten_color(color: Color32, amount: u8) -> Color32 {
    Color32::from_rgb(
        color.r().saturating_add(amount),
        color.g().saturating_add(amount),
        color.b().saturating_add(amount),
    )
}

#[allow(dead_code)]
fn _unused_stroke_kind_check() -> StrokeKind {
    StrokeKind::Inside
}
