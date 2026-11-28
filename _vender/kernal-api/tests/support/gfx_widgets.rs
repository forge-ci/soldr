//! Shared deterministic scene for live parity, offscreen tests and screenshots.
use super::gfx::{App, Column, ColumnAlign, Frame, TextRole, TextStyle};

pub struct StatusPanel;
impl App for StatusPanel {
    fn frame(&mut self, f: &mut Frame<'_>) {
        f.status("Building · 12 compiles", TextRole::Accent);
        f.heading("98% cache hits");
        f.styled_label(
            "Last build: soldr · 12.4s · 142 of 145 cached",
            TextStyle {
                role: TextRole::Secondary,
                small: true,
                ..TextStyle::default()
            },
        );
        f.separator();
        f.grid(
            "summary",
            &[
                Column {
                    width: 140.0,
                    align: ColumnAlign::Left,
                },
                Column {
                    width: 180.0,
                    align: ColumnAlign::Right,
                },
            ],
            |grid| {
                grid.cell(|f| f.label("Cache"));
                grid.cell(|f| f.label("4.2 of 25 GiB"));
                grid.cell(|f| f.label("Distributed"));
                grid.cell(|f| {
                    f.styled_label(
                        "2 nodes · 10 of 28 slots",
                        TextStyle {
                            strong: true,
                            ..TextStyle::default()
                        },
                    )
                });
            },
        )
        .expect("valid fixed columns");
        f.progress(0.168, Some("Cache space used"));
        f.styled_label(
            "Hit rate, last 5 builds · 98% latest",
            TextStyle {
                role: TextRole::Secondary,
                small: true,
                ..TextStyle::default()
            },
        );
        f.sparkline(&[0.82, 0.94, 0.89, 0.96, 0.98], 36.0, TextRole::Accent);
        f.separator();
        f.horizontal(|f| {
            f.button("Open dashboard");
            f.with_tooltip("Wait for the current operation to finish.", |f| {
                f.enabled(false, |f| f.button("Restart daemon"))
            });
        });
    }
}
