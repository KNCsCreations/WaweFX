use std::f32::consts::PI;

use eframe::egui::{
    self, Align, Color32, CornerRadius, Id, LayerId, Layout, Order, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};

use crate::audio::AudioEngine;
use crate::config::{AppConfig, SoundItem, rgb_to_color32};
use crate::ui::ThumbnailLoader;

/// User actions emitted by the soundboard pad grid.
#[derive(Debug, Default)]
pub struct GridAction {
    pub play_sound_id: Option<String>,
    pub stop_playback: bool,
    pub edit_sound_id: Option<String>,
    pub remove_sound_id: Option<String>,
    pub pad_rects: Vec<(String, Rect)>,
    pub config_dirty: bool,
}

/// Renders the responsive soundboard pad grid inside the main canvas area.
pub fn render_sound_grid(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    audio: &AudioEngine,
    thumbnails: &mut ThumbnailLoader,
    hovered_image_drop: bool,
) -> GridAction {
    let mut action = GridAction::default();

    // When there are no sounds yet, preserve the clean reference screenshot canvas cleanly.
    if config.sounds.is_empty() {
        return action;
    }

    let accent = rgb_to_color32(config.theme.accent_color);
    let card_bg = rgb_to_color32(config.theme.pad_card_bg);
    let card_hover = rgb_to_color32(config.theme.pad_card_hover);
    let text_color = rgb_to_color32(config.theme.pad_text);
    let icon_primary = rgb_to_color32(config.theme.icon_primary);

    let active_playing_id = audio.active_sound_id();
    let progress = audio.playback_progress();
    let selected_id = config.selected_sound_id.clone();

    let pad_min_width = 148.0;
    let spacing = 12.0;
    let avail_width = ui.available_width().max(pad_min_width);
    let cols = ((avail_width + spacing) / (pad_min_width + spacing))
        .floor()
        .max(1.0) as usize;
    let card_width = ((avail_width - spacing * (cols as f32 - 1.0)) / cols as f32).max(110.0);
    let card_height = card_width + 38.0;

    let drag_key = Id::new("wawefx_pad_drag_source");
    let mut dragging_id: Option<String> = ui.data_mut(|d| d.get_temp(drag_key));
    let pointer_pos = ui.input(|i| i.pointer.latest_pos().or(i.pointer.interact_pos()));
    let primary_down = ui.input(|i| i.pointer.primary_down());

    let sounds_snapshot: Vec<SoundItem> = config.sounds.clone();
    let dragging_src_idx = dragging_id
        .as_deref()
        .and_then(|id| sounds_snapshot.iter().position(|s| s.id == id));
    let mut drop_target_idx: Option<usize> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (row_idx, row) in sounds_snapshot.chunks(cols).enumerate() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = spacing;
                    for (col_idx, sound) in row.iter().enumerate() {
                        let flat_idx = row_idx * cols + col_idx;
                        let is_playing_this =
                            active_playing_id.as_deref() == Some(sound.id.as_str());
                        let is_selected = selected_id.as_deref() == Some(sound.id.as_str());
                        let is_being_dragged = dragging_id.as_deref() == Some(sound.id.as_str());

                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(card_width, card_height),
                            Sense::click_and_drag(),
                        );
                        action.pad_rects.push((sound.id.clone(), rect));

                        if resp.drag_started_by(egui::PointerButton::Primary) {
                            dragging_id = Some(sound.id.clone());
                        }

                        let pointer_over_card = pointer_pos.is_some_and(|p| rect.contains(p));
                        let is_image_drop_target = hovered_image_drop && pointer_over_card;

                        let fill = if is_being_dragged {
                            dim_color(card_bg, 0.55)
                        } else if resp.hovered() || is_image_drop_target {
                            card_hover
                        } else {
                            card_bg
                        };

                        let painter = ui.painter();

                        if is_playing_this {
                            painter.rect_filled(
                                rect.expand(2.5),
                                CornerRadius::same(12),
                                Color32::from_rgba_unmultiplied(
                                    accent.r(),
                                    accent.g(),
                                    accent.b(),
                                    85,
                                ),
                            );
                        }

                        painter.rect_filled(rect, CornerRadius::same(10), fill);

                        let border_stroke = if is_image_drop_target || is_playing_this {
                            Stroke::new(2.2_f32, accent)
                        } else if is_selected {
                            Stroke::new(1.8_f32, icon_primary)
                        } else {
                            Stroke::new(1.0_f32, Color32::from_black_alpha(32))
                        };
                        painter.rect_stroke(
                            rect,
                            CornerRadius::same(10),
                            border_stroke,
                            StrokeKind::Inside,
                        );

                        // Cover image / placeholder square
                        let img_margin = 8.0;
                        let img_size = (card_width - img_margin * 2.0).max(48.0);
                        let img_rect = Rect::from_min_size(
                            Pos2::new(rect.min.x + img_margin, rect.min.y + img_margin),
                            Vec2::new(img_size, img_size - 6.0),
                        );

                        if let Some(tex) = thumbnails.texture_for(ui.ctx(), sound) {
                            let tint = if is_being_dragged {
                                Color32::from_white_alpha(150)
                            } else {
                                Color32::WHITE
                            };
                            painter.image(
                                tex.id(),
                                img_rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                tint,
                            );
                            painter.rect_stroke(
                                img_rect,
                                CornerRadius::same(6),
                                Stroke::new(1.0_f32, Color32::from_black_alpha(28)),
                                StrokeKind::Inside,
                            );
                        } else {
                            painter.rect_filled(
                                img_rect,
                                CornerRadius::same(6),
                                Color32::from_black_alpha(24),
                            );
                            paint_waveform_placeholder(
                                painter,
                                img_rect.center(),
                                if is_playing_this { accent } else { icon_primary },
                            );
                        }

                        // Active playing badge & live bottom progress bar
                        if is_playing_this {
                            let badge_rect = Rect::from_min_size(
                                Pos2::new(img_rect.max.x - 48.0, img_rect.min.y + 5.0),
                                Vec2::new(43.0, 16.0),
                            );
                            painter.rect_filled(badge_rect, CornerRadius::same(4), accent);
                            painter.text(
                                badge_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "PLAYING",
                                egui::FontId::proportional(9.5),
                                Color32::WHITE,
                            );

                            let track = Rect::from_min_max(
                                Pos2::new(rect.min.x + 6.0, rect.max.y - 5.0),
                                Pos2::new(rect.max.x - 6.0, rect.max.y - 2.0),
                            );
                            painter.rect_filled(
                                track,
                                CornerRadius::same(2),
                                Color32::from_black_alpha(45),
                            );
                            let fill_w = track.width() * progress.clamp(0.0, 1.0);
                            if fill_w > 0.5 {
                                let filled = Rect::from_min_size(
                                    track.min,
                                    Vec2::new(fill_w, track.height()),
                                );
                                painter.rect_filled(filled, CornerRadius::same(2), accent);
                            }
                        }

                        // Insertion indicator bar when dragging another pad over this pad
                        if let Some(src_idx) = dragging_src_idx
                            && !is_being_dragged
                            && pointer_over_card
                        {
                            drop_target_idx = Some(flat_idx);
                            let bar_x = if src_idx < flat_idx {
                                rect.max.x + spacing * 0.5
                            } else {
                                rect.min.x - spacing * 0.5
                            };
                            painter.line_segment(
                                [
                                    Pos2::new(bar_x, rect.min.y + 4.0),
                                    Pos2::new(bar_x, rect.max.y - 4.0),
                                ],
                                Stroke::new(3.5_f32, accent),
                            );
                        }

                        // Title & duration row at the bottom of the card
                        let footer_rect = Rect::from_min_max(
                            Pos2::new(rect.min.x + 8.0, img_rect.max.y + 4.0),
                            Pos2::new(rect.max.x - 8.0, rect.max.y - 7.0),
                        );
                        let mut footer_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(footer_rect)
                                .layout(Layout::top_down(Align::LEFT)),
                        );
                        footer_ui.add(
                            egui::Label::new(
                                egui::RichText::new(&sound.title)
                                    .strong()
                                    .size(12.5)
                                    .color(text_color),
                            )
                            .truncate()
                            .selectable(false),
                        );
                        footer_ui.add(
                            egui::Label::new(
                                egui::RichText::new(sound.formatted_duration())
                                    .size(10.5)
                                    .color(Color32::from_rgba_unmultiplied(
                                        text_color.r(),
                                        text_color.g(),
                                        text_color.b(),
                                        185,
                                    )),
                            )
                            .selectable(false),
                        );

                        // Left-click (without drag): select AND play immediately
                        if resp.clicked() {
                            config.selected_sound_id = Some(sound.id.clone());
                            action.play_sound_id = Some(sound.id.clone());
                            action.config_dirty = true;
                        }

                        // Right-click: select and open context menu
                        if resp.secondary_clicked() {
                            config.selected_sound_id = Some(sound.id.clone());
                            action.config_dirty = true;
                        }

                        resp.context_menu(|ui| {
                            if ui.button("Play").clicked() {
                                action.play_sound_id = Some(sound.id.clone());
                                ui.close_menu();
                            }
                            if is_playing_this && ui.button("Stop").clicked() {
                                action.stop_playback = true;
                                ui.close_menu();
                            }
                            if ui.button("Edit Sound (...)").clicked() {
                                action.edit_sound_id = Some(sound.id.clone());
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Remove Sound (-)").clicked() {
                                action.remove_sound_id = Some(sound.id.clone());
                                ui.close_menu();
                            }
                        });
                    }
                });
                ui.add_space(spacing);
            }
        });

    // Finalize drag ghost preview or drop-to-reorder
    if let Some(ref src_id) = dragging_id {
        if primary_down {
            ui.data_mut(|d| d.insert_temp(drag_key, Some(src_id.clone())));
            if let (Some(pos), Some(src_idx)) = (pointer_pos, dragging_src_idx) {
                let ghost_painter = ui
                    .ctx()
                    .layer_painter(LayerId::new(Order::Tooltip, Id::new("pad_drag_ghost")));
                let title = &sounds_snapshot[src_idx].title;
                let pill = Rect::from_center_size(pos + Vec2::new(14.0, -14.0), Vec2::new(130.0, 26.0));
                ghost_painter.rect_filled(pill, CornerRadius::same(7), card_hover);
                ghost_painter.rect_stroke(
                    pill,
                    CornerRadius::same(7),
                    Stroke::new(1.5_f32, accent),
                    StrokeKind::Inside,
                );
                ghost_painter.text(
                    pill.center(),
                    egui::Align2::CENTER_CENTER,
                    title,
                    egui::FontId::proportional(11.5),
                    text_color,
                );
            }
        } else {
            ui.data_mut(|d| d.remove::<Option<String>>(drag_key));
            if let (Some(src_idx), Some(target_idx)) = (dragging_src_idx, drop_target_idx)
                && src_idx != target_idx
                && src_idx < config.sounds.len()
                && target_idx < config.sounds.len()
            {
                let item = config.sounds.remove(src_idx);
                config.sounds.insert(target_idx, item);
                action.config_dirty = true;
            }
        }
    }

    action
}

fn dim_color(color: Color32, factor: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * factor).clamp(0.0, 255.0) as u8,
    )
}

fn paint_waveform_placeholder(painter: &egui::Painter, center: Pos2, color: Color32) {
    let bars = [-16.0f32, -8.0, 0.0, 8.0, 16.0];
    for (idx, dx) in bars.iter().enumerate() {
        let phase = (idx as f32 / (bars.len() - 1) as f32) * PI;
        let half_h = 6.0 + 11.0 * phase.sin();
        painter.line_segment(
            [
                Pos2::new(center.x + dx, center.y - half_h),
                Pos2::new(center.x + dx, center.y + half_h),
            ],
            Stroke::new(3.2_f32, color),
        );
    }
}
