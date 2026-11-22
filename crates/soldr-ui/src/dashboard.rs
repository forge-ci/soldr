use crate::{
    data::{self, Connection, PeerKind},
    model::*,
    ui,
    view::{Panel, Tab},
};
use kernal_api::gfx::Frame;

pub fn draw(panel: &mut Panel, f: &mut Frame<'_>) {
    ui::status_row(f, &panel.state, panel.scale);
    f.space(8.0);
    let tabs = [
        ("Overview", Tab::Overview),
        ("Builds", Tab::Builds),
        ("Dist nodes", Tab::Nodes),
        ("Diagnostics", Tab::Diagnostics),
    ];
    let labels: Vec<_> = tabs
        .iter()
        .map(|(s, tab)| {
            if *tab == panel.tab {
                format!("• {s}")
            } else {
                (*s).into()
            }
        })
        .collect();
    let refs: Vec<_> = labels.iter().map(String::as_str).collect();
    if let Some(index) = ui::row_buttons(f, &refs, panel.scale) {
        panel.tab = tabs[index].1;
    }
    f.separator();
    match panel.tab {
        Tab::Overview => {
            panel.hero(f);
            let builds = data::builds(&panel.state);
            if let Some(line) = data::sparkline(&builds) {
                ui::sparkline(f, &line, panel.scale);
            }
            if let Some(cache) = data::cache_label(&panel.state) {
                f.label(&cache);
            }
            f.space(8.0);
            build_table(panel, f, 4);
            if panel.state.daemon() != DaemonState::Stopped {
                f.separator();
                nodes(panel, f, 4);
            }
            f.separator();
            panel.controls(f, true);
            problems(panel, f);
        }
        Tab::Builds => {
            build_table(panel, f, 12);
            if let Some(index) = panel.selected_build {
                if let Some(build) = data::builds(&panel.state).get(index) {
                    f.separator();
                    f.heading(&build.project);
                    f.label(&build.summary());
                }
            }
        }
        Tab::Nodes => {
            nodes(panel, f, usize::MAX);
            panel.dist_controls(f);
            problems(panel, f);
        }
        Tab::Diagnostics => diagnostics(panel, f),
    }
    panel.feedback(f);
}
fn build_table(panel: &mut Panel, f: &mut Frame<'_>, limit: usize) {
    f.heading("Recent builds");
    let builds = data::builds(&panel.state);
    if builds.is_empty() {
        f.label("No builds yet.");
        return;
    }
    let wide = f.available_width() >= 620.0 * panel.scale;
    if wide {
        ui::grid_row(
            f,
            &[
                (0.0, "Project"),
                (0.40, "Duration"),
                (0.58, "Cached"),
                (0.82, "Hit rate"),
            ],
            false,
            panel.scale,
        );
        f.separator();
    }
    f.list(|f| {
        for (index, build) in builds.iter().take(limit).enumerate() {
            if wide {
                let rate = build
                    .rate
                    .map_or_else(String::new, |r| format!("{:.0}%", r * 100.0));
                if ui::grid_row(
                    f,
                    &[
                        (0.0, &build.project),
                        (0.40, &build.duration_label()),
                        (0.58, &build.cached_label()),
                        (0.82, &rate),
                    ],
                    true,
                    panel.scale,
                ) {
                    panel.selected_build = Some(index);
                    panel.tab = Tab::Builds;
                }
            } else {
                if f.button(&build.project) {
                    panel.selected_build = Some(index);
                    panel.tab = Tab::Builds;
                }
                let details = [
                    build.duration_label(),
                    build.cached_label(),
                    build
                        .rate
                        .map_or_else(String::new, |r| format!("{:.0}% cache hits", r * 100.0)),
                ];
                f.label(
                    &details
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        }
    });
}
fn nodes(panel: &Panel, f: &mut Frame<'_>, limit: usize) {
    if panel.state.running() == Some(false) {
        f.label("Start soldr to use distributed builds.");
        return;
    }
    match panel.state.dist {
        DistCapability::Unsupported => {
            f.label(&data::version(&panel.state).map_or_else(
                || "Distributed builds aren't included in this soldr build.".into(),
                |v| format!("Distributed builds aren't included in soldr {v}."),
            ));
            return;
        }
        DistCapability::Unknown => {
            f.label("Checking distributed builds…");
            return;
        }
        DistCapability::Off => {}
        _ => {}
    }
    f.heading("Distributed builds");
    if panel.state.dist == DistCapability::Off {
        f.label("Distributed builds are off.");
    }
    let peers = data::peers(&panel.state);
    for (kind, title) in [
        (PeerKind::Helper, "Helper nodes this build uses"),
        (PeerKind::Client, "Connected clients using this machine"),
    ] {
        let rows: Vec<_> = peers.iter().filter(|p| p.kind == kind).collect();
        if rows.is_empty() {
            continue;
        }
        f.label(title);
        let wide = f.available_width() >= 650.0 * panel.scale;
        if wide {
            ui::grid_row(
                f,
                &[
                    (0.0, "Device"),
                    (0.40, "Slots"),
                    (0.70, "Latency"),
                    (0.84, "Connection"),
                ],
                false,
                panel.scale,
            );
        }
        for peer in rows.into_iter().take(limit) {
            let slots = match (peer.used, peer.total) {
                (Some(used), Some(total)) => {
                    format!("{}  {used}/{total}", data::progress_glyphs(used, total, 8))
                }
                _ => String::new(),
            };
            let latency = peer.latency.map_or_else(String::new, |n| format!("{n} ms"));
            let connection = match peer.connection {
                Connection::Connected => "Connected",
                Connection::Connecting => "Connecting",
                Connection::Disconnected => "Disconnected",
                Connection::Unknown => "",
            };
            if wide {
                ui::grid_row(
                    f,
                    &[
                        (0.0, &peer.name),
                        (0.40, &slots),
                        (0.70, &latency),
                        (0.84, connection),
                    ],
                    false,
                    panel.scale,
                );
            } else {
                f.label(&peer.name);
                f.label(
                    &[slots, latency, connection.into()]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        }
        f.space(8.0);
    }
    if let Some(text) = data::contribution(&panel.state) {
        f.label(&text);
    }
}
fn diagnostics(panel: &mut Panel, f: &mut Frame<'_>) {
    f.heading("Diagnostics");
    if let Some(version) = data::version(&panel.state) {
        f.label(&format!("soldr {version}"));
    }
    for source in Source::ALL {
        if let Some(sample) = panel.state.samples.get(&source) {
            match &sample.freshness {
                Freshness::Unsupported => {}
                Freshness::Fresh { at } => f.label(&format!(
                    "{} · updated {} ago",
                    source.label(),
                    age(panel.state.now.saturating_sub(*at))
                )),
                Freshness::Stale { at, .. } => f.label(&at.map_or_else(
                    || format!("{} · waiting for data", source.label()),
                    |at| {
                        format!(
                            "{} · last updated {} ago",
                            source.label(),
                            age(panel.state.now.saturating_sub(at))
                        )
                    },
                )),
                Freshness::Loading => f.label(&format!("{} · loading", source.label())),
            }
        }
    }
    f.separator();
    panel.controls(f, true);
    problems(panel, f);
    if panel.state.problems().is_empty()
        && f.button(if panel.details {
            "Hide details"
        } else {
            "Show details"
        })
    {
        panel.details = !panel.details;
    }
    if panel.details {
        raw_details(panel, f);
    }
}
fn problems(panel: &mut Panel, f: &mut Frame<'_>) {
    let problems = panel.state.problems();
    if problems.is_empty() {
        return;
    }
    f.separator();
    f.heading("Problems");
    for row in problems {
        f.label(&row);
    }
    if f.button(if panel.details {
        "Hide details"
    } else {
        "Show details"
    }) {
        panel.details = !panel.details;
    }
    if panel.details && panel.tab != Tab::Diagnostics {
        raw_details(panel, f);
    }
}
fn raw_details(panel: &Panel, f: &mut Frame<'_>) {
    for source in Source::ALL {
        let Some(sample) = panel.state.samples.get(&source) else {
            continue;
        };
        if sample.freshness == Freshness::Unsupported {
            continue;
        }
        if panel.tab == Tab::Diagnostics || matches!(sample.freshness, Freshness::Stale { .. }) {
            f.label(source.label());
            if let Freshness::Stale { reason, .. } = &sample.freshness {
                f.label(reason);
                continue;
            }
            if !sample.reply.stdout.is_empty() {
                f.label(&sample.reply.stdout);
            }
            if !sample.reply.stderr.is_empty() {
                f.label(&sample.reply.stderr);
            }
        }
    }
    match &panel.state.action {
        ActionState::Succeeded { stdout, stderr, .. }
        | ActionState::Failed { stdout, stderr, .. } => {
            if !stdout.is_empty() {
                f.label(stdout);
            }
            if !stderr.is_empty() {
                f.label(stderr);
            }
        }
        _ => {}
    }
}
