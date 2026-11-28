//! Private adapters for utility-panel widgets.
use super::{apply_theme, color32, FrameImp};
use crate::gfx::{Column, ColumnAlign, TextRole, TextStyle, Theme, ThemeMode};
use std::sync::atomic::{AtomicU8, Ordering};

static SYSTEM_THEME: AtomicU8 = AtomicU8::new(0);

pub(super) fn observe_system_theme(ctx: &egui::Context) {
    let value = match ctx.system_theme() {
        None => 0,
        Some(egui::Theme::Dark) => 1,
        Some(egui::Theme::Light) => 2,
    };
    SYSTEM_THEME.store(value, Ordering::Relaxed);
}

pub(crate) fn system_theme() -> Option<ThemeMode> {
    match SYSTEM_THEME.load(Ordering::Relaxed) {
        1 => Some(ThemeMode::Dark),
        2 => Some(ThemeMode::Light),
        _ => None,
    }
}

pub(super) fn refresh_system_theme(ui: &mut egui::Ui) {
    let theme = ui
        .ctx()
        .data(|data| data.get_temp::<Theme>(egui::Id::unique("kernal-theme")));
    if let Some(theme) = theme {
        if theme.mode == ThemeMode::FollowSystem {
            apply_theme(ui.ctx(), theme);
            ui.set_style(ui.ctx().global_style());
        }
    }
}

impl FrameImp<'_> {
    fn widget_theme(&self) -> Theme {
        let mut theme = self
            .ui
            .ctx()
            .data(|data| data.get_temp::<Theme>(egui::Id::unique("kernal-theme")))
            .unwrap_or_default();
        theme.mode = if self.ui.visuals().dark_mode {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        theme
    }

    pub(crate) fn enabled(&mut self, enabled: bool, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        self.ui
            .add_enabled_ui(enabled, |ui| body(FrameImp { ui, shared }));
    }

    pub(crate) fn styled_label(&mut self, text: &str, style: TextStyle) {
        let color = style
            .color
            .unwrap_or_else(|| self.widget_theme().role_color(style.role));
        let mut rich = egui::RichText::new(text).color(color32(color));
        if style.small {
            rich = rich.small();
        }
        let (pos, galley, response) = egui::Label::new(rich)
            .sense(egui::Sense::hover())
            .selectable(false)
            .layout_in_ui(self.ui);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Label, self.ui.is_enabled(), text)
        });
        if self.ui.is_rect_visible(response.rect) {
            self.ui
                .painter()
                .galley(pos, galley.clone(), color32(color));
            if style.strong {
                // egui's `strong()` changes colour only. Embolden the glyphs
                // without replacing semantic colour or bundling a second font.
                self.ui
                    .painter()
                    .galley(pos + egui::vec2(0.45, 0.0), galley, color32(color));
            }
        }
    }

    pub(crate) fn status(&mut self, text: &str, role: TextRole) {
        let color = color32(self.widget_theme().role_color(role));
        self.ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(color))
                    .sense(egui::Sense::hover()),
            );
        });
    }

    pub(crate) fn progress(&mut self, fraction: f32, text: Option<&str>) {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let bar = egui::ProgressBar::new(fraction);
        // A single foreground must contrast with both the filled and unfilled
        // track. Plain adjacent text avoids making that impossible for accents.
        self.ui.add(bar);
        if let Some(text) = text {
            self.ui.label(text);
        } else {
            self.ui.label(format!("{:.0}%", fraction * 100.0));
        }
    }

    pub(crate) fn with_tooltip(&mut self, text: &str, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        let response = self.ui.scope(|ui| body(FrameImp { ui, shared })).response;
        response.on_hover_text(text).on_disabled_hover_text(text);
    }

    pub(crate) fn grid(&mut self, id: &str, count: usize, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        egui::Grid::new(id)
            .num_columns(count)
            .min_col_width(0.0)
            .show(self.ui, |ui| body(FrameImp { ui, shared }));
    }

    pub(crate) fn grid_cell(&mut self, column: Column, body: &mut dyn FnMut(FrameImp<'_>)) {
        let shared = &mut *self.shared;
        let align = match column.align {
            ColumnAlign::Left => egui::Align::Min,
            ColumnAlign::Right => egui::Align::Max,
        };
        self.ui.allocate_ui_with_layout(
            egui::vec2(column.width, 0.0),
            egui::Layout::top_down(align),
            |ui| {
                ui.set_min_width(column.width);
                ui.set_max_width(column.width);
                body(FrameImp { ui, shared });
            },
        );
    }

    pub(crate) fn end_row(&mut self) {
        self.ui.end_row();
    }

    pub(crate) fn sparkline(&mut self, samples: &[f32], height: f32, role: TextRole) {
        if !height.is_finite() || height <= 0.0 {
            return;
        }
        let (rect, _) = self.ui.allocate_exact_size(
            egui::vec2(self.ui.available_width(), height),
            egui::Sense::hover(),
        );
        let stroke = egui::Stroke::new(1.5, color32(self.widget_theme().role_color(role)));
        for segment in sparkline_segments(samples, rect.shrink((height / 2.0).min(1.0))) {
            self.ui.painter_at(rect).line_segment(segment, stroke);
        }
    }
}

fn sparkline_segments(samples: &[f32], rect: egui::Rect) -> Vec<[egui::Pos2; 2]> {
    let (min, max) = samples
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(min, max), v| {
            (min.min(v), max.max(v))
        });
    let span = f64::from(max) - f64::from(min);
    let mut previous = None;
    let mut segments = Vec::new();
    for (index, &value) in samples.iter().enumerate() {
        if !value.is_finite() {
            previous = None;
            continue;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "mini-chart pixel resolution is f32"
        )]
        let x = index as f32 / samples.len().saturating_sub(1).max(1) as f32;
        #[allow(clippy::cast_possible_truncation, reason = "normalized to 0..=1")]
        let y = if span > 0.0 {
            ((f64::from(value) - f64::from(min)) / span) as f32
        } else {
            0.5
        };
        let point = egui::pos2(
            rect.left() + x * rect.width(),
            rect.bottom() - y * rect.height(),
        );
        if let Some(previous) = previous {
            segments.push([previous, point]);
        }
        previous = Some(point);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::{Frame, GfxErrorKind};
    use crate::gfx_egui::{
        tests::{click, frame},
        Shared,
    };

    #[test]
    fn disabled_scope_preserves_values_and_cannot_be_reenabled_by_a_child() {
        let ctx = egui::Context::default();
        let mut value = false;
        let mut text = String::from("unchanged");
        let mut secret = String::from("private");
        let at = egui::pos2(20.0, 8.0);
        for events in [
            vec![],
            vec![egui::Event::PointerMoved(at)],
            vec![click(at, true)],
            vec![click(at, false)],
            vec![egui::Event::Text("no".into())],
        ] {
            frame(&ctx, events, |f| {
                let result = f.enabled(false, |f| {
                    assert!(!f.button("disabled"));
                    f.enabled(true, |f| {
                        assert!(!f.toggle(&mut value, "also disabled"));
                        assert!(!f.text_field(&mut text));
                        assert!(!f.secret_field(&mut secret, false));
                    });
                    42
                });
                assert_eq!(result, 42);
            });
        }
        assert!(!value);
        assert_eq!(text, "unchanged");
        assert_eq!(secret, "private");
        assert!(ctx.memory(|m| m.focused().is_none()));
        frame(&ctx, vec![], |f| assert!(f.imp.ui.is_enabled()));
    }

    #[test]
    fn grid_widths_alignment_rows_and_returns_are_stable_across_frames() {
        let ctx = egui::Context::default();
        let columns = [
            Column {
                width: 80.0,
                align: ColumnAlign::Left,
            },
            Column {
                width: 160.0,
                align: ColumnAlign::Right,
            },
        ];
        let mut positions = Vec::new();
        for value in ["1", "100000", "7"] {
            frame(&ctx, vec![], |f| {
                f.grid("metrics", &columns, |g| {
                    assert_eq!(
                        g.cell(|f| {
                            f.label("Cache");
                            3
                        }),
                        3
                    );
                    g.cell(|f| {
                        assert!((f.available_width() - 160.0).abs() < 0.01);
                        assert_eq!(f.imp.ui.layout().horizontal_align(), egui::Align::Max);
                        f.label(value);
                        positions.push(f.imp.ui.min_rect().right());
                    });
                    g.cell(|f| f.label("Next row"));
                    g.cell(|f| f.label("second"));
                })
                .unwrap();
                let mut invoked = false;
                let error = f
                    .grid("invalid", &[], |_| {
                        invoked = true;
                    })
                    .unwrap_err();
                assert_eq!(error.kind(), GfxErrorKind::InvalidOptions);
                assert!(!invoked);
            });
        }
        assert!(
            positions
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() < 0.01),
            "{positions:?}"
        );
    }

    #[test]
    fn sparkline_handles_empty_constant_gaps_and_extreme_values() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 20.0));
        assert!(sparkline_segments(&[], rect).is_empty());
        assert!(sparkline_segments(&[1.0], rect).is_empty());
        let flat = sparkline_segments(&[2.0, 2.0], rect);
        assert_eq!(flat, vec![[egui::pos2(0.0, 10.0), egui::pos2(100.0, 10.0)]]);
        assert!(sparkline_segments(&[1.0, f32::NAN, 2.0], rect).is_empty());
        let extreme = sparkline_segments(&[f32::MIN, f32::MAX], rect);
        assert_eq!(
            extreme,
            vec![[egui::pos2(0.0, 20.0), egui::pos2(100.0, 0.0)]]
        );
    }

    #[test]
    fn system_theme_query_reports_unknown_light_and_dark_observations() {
        let ctx = egui::Context::default();
        let saved = SYSTEM_THEME.load(Ordering::Relaxed);
        for (system, expected) in [
            (None, None),
            (Some(egui::Theme::Light), Some(ThemeMode::Light)),
            (Some(egui::Theme::Dark), Some(ThemeMode::Dark)),
        ] {
            let input = egui::RawInput {
                system_theme: system,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| observe_system_theme(ui.ctx()));
            assert_eq!(crate::gfx::system_theme(), expected);
            output.textures_delta.clear();
        }
        SYSTEM_THEME.store(saved, Ordering::Relaxed);
    }

    #[test]
    fn follows_system_changes_without_affecting_explicit_modes() {
        let ctx = egui::Context::default();
        for mode in [ThemeMode::FollowSystem, ThemeMode::Dark, ThemeMode::Light] {
            apply_theme(&ctx, Theme::with_mode(mode));
            if mode == ThemeMode::FollowSystem {
                assert_eq!(
                    ctx.options(|o| o.theme_preference),
                    egui::ThemePreference::System
                );
            }
            for system in [Some(egui::Theme::Light), Some(egui::Theme::Dark), None] {
                let input = egui::RawInput {
                    system_theme: system,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| {
                    refresh_system_theme(ui);
                    let expected = match mode {
                        ThemeMode::FollowSystem => system != Some(egui::Theme::Light),
                        _ => mode == ThemeMode::Dark,
                    };
                    assert_eq!(ui.visuals().dark_mode, expected);
                });
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn styled_label_uses_requested_color_size_and_emphasis() {
        let ctx = egui::Context::default();
        let theme = Theme::default();
        apply_theme(&ctx, theme);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut shared = Shared::new(vec![]);
            let mut f = Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            f.styled_label(
                "warning",
                TextStyle {
                    role: TextRole::Warning,
                    strong: true,
                    small: true,
                    color: None,
                },
            );
            f.styled_label(
                "custom",
                TextStyle {
                    color: Some(crate::gfx::Color::rgb(12, 34, 56)),
                    ..TextStyle::default()
                },
            );
        });
        let texts: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(&text.galley.job),
                _ => None,
            })
            .collect();
        let warning = texts.iter().find(|job| job.text == "warning").unwrap();
        let format = &warning.sections[0].format;
        assert_eq!(format.color, color32(theme.role_color(TextRole::Warning)));
        assert!((format.font_id.size - theme.text_size * 0.75).abs() < 0.01);
        assert_eq!(
            texts.iter().filter(|job| job.text == "warning").count(),
            2,
            "strong paints an extra offset glyph pass; semantic colour alone is not weight"
        );
        let custom = texts.iter().find(|job| job.text == "custom").unwrap();
        assert_eq!(
            custom.sections[0].format.color,
            egui::Color32::from_rgb(12, 34, 56)
        );
        output.textures_delta.clear();
    }

    #[test]
    fn progress_clamps_fractions_and_replaces_percentage_with_text() {
        let ctx = egui::Context::default();
        for (fraction, expected) in [(f32::NAN, "0%"), (-1.0, "0%"), (0.5, "50%"), (2.0, "100%")] {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut shared = Shared::new(vec![]);
                let mut f = Frame {
                    imp: FrameImp {
                        ui,
                        shared: &mut shared,
                    },
                };
                f.progress(fraction, None);
            });
            assert!(format!("{:?}", output.shapes).contains(expected));
            output.textures_delta.clear();
        }
    }

    #[test]
    fn tooltip_appears_over_a_disabled_control() {
        let ctx = egui::Context::default();
        ctx.global_style_mut(|style| style.interaction.tooltip_delay = 0.0);
        let mut at = egui::pos2(10.0, 10.0);
        let mut shown = false;
        for tick in 0..5 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                time: Some(f64::from(tick)),
                events: if tick == 1 {
                    vec![egui::Event::PointerMoved(at)]
                } else {
                    vec![]
                },
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut shared = Shared::new(vec![]);
                let mut f = Frame {
                    imp: FrameImp {
                        ui,
                        shared: &mut shared,
                    },
                };
                f.with_tooltip("Cannot restart while saving", |f| {
                    f.enabled(false, |f| {
                        assert!(!f.button("Restart"));
                        at = f.imp.ui.min_rect().center();
                    });
                });
            });
            shown |= format!("{:?}", output.shapes).contains("Cannot restart while saving");
            output.textures_delta.clear();
        }
        assert!(shown, "disabled controls retain explanatory tooltips");
    }

    #[test]
    fn widget_text_style_status_progress_and_tooltip_are_noninteractive() {
        let ctx = egui::Context::default();
        apply_theme(&ctx, Theme::default());
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut shared = Shared::new(vec![]);
            let mut f = Frame {
                imp: FrameImp {
                    ui,
                    shared: &mut shared,
                },
            };
            f.styled_label(
                "styled",
                TextStyle {
                    role: TextRole::Warning,
                    strong: true,
                    small: true,
                    color: None,
                },
            );
            f.status("Ready", TextRole::Success);
            assert!(f.imp.ui.ctx().memory(|m| m.focused().is_none()));
            for fraction in [f32::NAN, -1.0, 0.5, 2.0] {
                f.progress(fraction, Some("Working"));
            }
            let result = f.with_tooltip("explanation", |f| {
                f.enabled(false, |f| f.button("disabled"))
            });
            assert!(!result);
            let before = f.available_height();
            f.sparkline(&[1.0, 2.0, 1.0], 20.0, TextRole::Accent);
            assert!(before - f.available_height() >= 20.0);
        });
        let shapes = format!("{:?}", output.shapes);
        assert!(
            shapes.contains("styled") && shapes.contains("Ready") && shapes.contains("Working")
        );
        output.textures_delta.clear();
    }
}
