use crate::{
    data,
    host::Controller,
    model::*,
    ui,
    view::{self, Confirm, HostEvent, Mode, Panel, Tab},
    worker::{self, RefreshPolicy},
};
use kernal_api::gfx::{self, Color};
const FIXTURES: [(&str, &str); 10] = [
    (
        "healthy-idle",
        include_str!("../fixtures/healthy-idle.json"),
    ),
    ("busy-dist", include_str!("../fixtures/busy-dist.json")),
    ("daemon-down", include_str!("../fixtures/daemon-down.json")),
    (
        "dist-unavailable",
        include_str!("../fixtures/dist-unavailable.json"),
    ),
    ("building", include_str!("../fixtures/building.json")),
    (
        "stopping-pending",
        include_str!("../fixtures/stopping-pending.json"),
    ),
    (
        "action-failed",
        include_str!("../fixtures/action-failed.json"),
    ),
    (
        "narrow-window",
        include_str!("../fixtures/narrow-window.json"),
    ),
    ("large-text", include_str!("../fixtures/large-text.json")),
    ("stale-data", include_str!("../fixtures/stale-data.json")),
];
fn fixture(index: usize) -> State {
    serde_json::from_str::<State>(FIXTURES[index].1)
        .unwrap()
        .hydrate()
}
fn reply(value: serde_json::Value) -> Reply {
    Reply {
        stdout: value.to_string(),
        stderr: String::new(),
        success: true,
    }
}
#[test]
fn every_cli_source_parses_and_rejects_invalid_shapes() {
    let state = fixture(0);
    for source in Source::ALL {
        assert!(parse(source, &state.sources[&source]).unwrap().is_object());
        assert!(parse(source, &reply(serde_json::json!({}))).is_err());
        assert!(parse(
            source,
            &Reply {
                stdout: "not JSON".into(),
                ..Reply::default()
            }
        )
        .is_err());
    }
}
#[test]
fn capability_is_not_a_health_or_freshness_error() {
    let state = fixture(3);
    assert_eq!(state.dist, DistCapability::Unsupported);
    assert_eq!(state.daemon(), DaemonState::Ready);
    assert!(state.stale_rows().is_empty());
    assert!(state.problems().is_empty());
    assert!(state.samples[&Source::Dist].reply.stderr.is_empty());
    assert!(!state.can_submit(Action::Share(true)));
    assert!(!state.can_submit(Action::Pair));
}
#[test]
fn daemon_states_and_action_availability_are_typed() {
    let idle = fixture(0);
    assert_eq!(idle.daemon(), DaemonState::Ready);
    assert!(!idle.can_submit(Action::Start));
    assert!(idle.can_submit(Action::Stop));
    let busy = fixture(1);
    assert_eq!(busy.daemon(), DaemonState::Building { n: 12 });
    let mut stopped = fixture(2);
    assert_eq!(stopped.daemon(), DaemonState::Stopped);
    assert!(!stopped.can_submit(Action::Stop));
    assert!(!stopped.can_submit(Action::Restart));
    assert!(stopped.begin(Action::Start));
    assert_eq!(stopped.daemon(), DaemonState::Starting);
    assert!(!stopped.begin(Action::Start));
    assert!(!stopped.can_submit(Action::Clean));
    assert_eq!(State::default().daemon(), DaemonState::Connecting);
}
#[test]
fn refresh_generation_drops_old_replies_and_retains_last_good_age() {
    let mut state = fixture(0);
    let old = state.generation;
    state.next_generation();
    assert!(!state.apply(
        old,
        Source::Daemon,
        reply(serde_json::json!({"running":false}))
    ));
    assert_eq!(state.daemon(), DaemonState::Ready);
    state.now += 125;
    let gen = state.generation;
    state.apply(
        gen,
        Source::Daemon,
        Reply {
            stderr: "socket refused".into(),
            ..Reply::default()
        },
    );
    assert_eq!(state.value(Source::Daemon)["running"], true);
    assert!(matches!(state.daemon(), DaemonState::NeedsAttention { .. }));
    assert!(state.stale_rows().iter().any(|s| s.contains("2m ago")));
    assert!(state.samples[&Source::Daemon]
        .reply
        .stderr
        .contains("socket refused"));
    assert!(!state.can_submit(Action::Restart));
}
#[test]
fn sharing_intent_does_not_snap_back_and_failures_reconcile() {
    let mut state = fixture(1);
    assert_eq!(state.dist, DistCapability::Client);
    assert!(!state.sharing.confirmed);
    assert!(state.begin(Action::Share(true)));
    assert!(state.sharing.displayed());
    let gen = state.generation;
    let old = state.sources[&Source::Dist].clone();
    state.apply(gen, Source::Dist, old);
    assert!(state.sharing.displayed());
    assert!(state.sharing.pending);
    worker::finish(
        &mut state,
        Action::Share(true),
        worker::Outcome {
            success: true,
            message: "Sharing requested.".into(),
            stdout: String::new(),
            stderr: String::new(),
            partial_restart: false,
        },
    );
    assert!(state.sharing.awaiting_refresh);
    assert!(state.sharing.displayed());
    assert!(!state.can_submit(Action::Clean));
    let gen = state.generation;
    state.apply(
        gen,
        Source::Dist,
        reply(serde_json::json!({"role":"node","sharing_enabled":true})),
    );
    assert!(state.sharing.confirmed);
    assert_eq!(state.sharing.requested, None);
    assert!(state.begin(Action::Share(false)));
    worker::finish(
        &mut state,
        Action::Share(false),
        worker::Outcome {
            success: false,
            message: "Couldn't change sharing.".into(),
            stdout: String::new(),
            stderr: "denied".into(),
            partial_restart: false,
        },
    );
    assert!(state.sharing.displayed());
    assert!(!state.sharing.pending);
}
#[test]
fn partial_restart_explains_stopped_state_and_keeps_both_outputs() {
    let stopped = Reply {
        success: true,
        stdout: "stopped the old daemon".into(),
        stderr: "stop warning".into(),
    };
    let started = Reply {
        success: false,
        stdout: "start attempted".into(),
        stderr: "permission denied".into(),
    };
    let outcome = worker::restart_outcome(stopped, Some(started));
    assert!(outcome.partial_restart);
    assert!(outcome.message.contains("stopped"));
    assert!(outcome.stdout.contains("stopped the old daemon"));
    assert!(outcome.stdout.contains("start attempted"));
    assert!(outcome.stderr.contains("stop warning"));
    assert!(outcome.stderr.contains("permission denied"));
    let mut state = fixture(0);
    worker::finish(&mut state, Action::Restart, outcome);
    assert_eq!(state.daemon(), DaemonState::Stopped);
    assert!(state.can_submit(Action::Start));
    assert!(!state.can_submit(Action::Restart));
}
#[test]
fn compositor_heartbeat_pauses_fast_polling_when_frames_stop() {
    let mut state = fixture(0);
    state.visible = true;
    state.last_frame_at = state.now;
    assert!(state.polling_visible());
    state.tick(state.now + 6);
    assert!(!state.polling_visible());
    state.last_frame_at = state.now;
    assert!(state.polling_visible());
    state.visible = false;
    assert!(!state.polling_visible());
}

#[test]
fn doctor_frequency_and_visibility_are_bounded() {
    let mut policy = RefreshPolicy::default();
    let first = policy.due(1000, true, false);
    assert!(first.contains(&Source::Doctor));
    assert!(first.contains(&Source::Dist));
    policy.dist_supported = Some(false);
    let next = policy.due(1002, true, false);
    assert!(!next.contains(&Source::Doctor));
    assert!(!next.contains(&Source::Dist));
    assert!(!policy.due(1059, true, false).contains(&Source::Doctor));
    assert!(policy.due(1060, true, false).contains(&Source::Doctor));
    assert_eq!(policy.due(1061, false, false), [Source::Daemon]);
    assert!(policy.due(1063, false, false).is_empty());
}
#[test]
fn escape_cancels_confirmation_before_closing_and_tray_starts_silent() {
    assert_eq!(
        view::confirmation(&fixture(1), Action::Stop),
        Some(Confirm::Stop)
    );
    assert_eq!(view::confirmation(&fixture(0), Action::Stop), None);
    assert_eq!(
        view::confirmation(&fixture(0), Action::Clean),
        Some(Confirm::Clean)
    );
    let mut panel = Panel::fixture(fixture(1), Mode::Panel);
    panel.confirm = Some(Confirm::Stop);
    assert!(!panel.escape());
    assert_eq!(panel.confirm, None);
    assert!(panel.escape());
    let mut controller = Controller::default();
    assert_eq!(controller.open, None);
    for _ in 0..50 {
        assert_eq!(controller.event(HostEvent::Toggle), Some(Mode::Panel));
        assert_eq!(controller.event(HostEvent::Toggle), None);
    }
    assert_eq!(
        controller.event(HostEvent::Dashboard),
        Some(Mode::Dashboard)
    );
    assert_eq!(
        controller.event(HostEvent::Dashboard),
        Some(Mode::Dashboard)
    );
}
#[test]
fn presentation_omits_missing_numbers_and_tracks_helpers_separately() {
    let idle = fixture(0);
    assert_eq!(
        data::session_rate(&idle).map(|r| (r * 100.0).round() as u64),
        Some(98)
    );
    assert!(data::contribution(&idle).is_none());
    assert!(data::sparkline(&data::builds(&idle)).is_none());
    let busy = fixture(1);
    assert!(data::sparkline(&data::builds(&busy)).is_some());
    assert_eq!(data::peers(&busy).len(), 2);
    assert!(!busy.sharing.confirmed);
    let node = fixture(4);
    assert!(node.sharing.confirmed);
    assert_eq!(data::peers(&node)[0].kind, data::PeerKind::Client);
}
#[test]
fn all_palette_text_meets_wcag_aa_on_panel_and_control_backgrounds() {
    let mut states: Vec<_> = (0..FIXTURES.len()).map(fixture).collect();
    let mut needs_attention = fixture(0);
    let generation = needs_attention.generation;
    needs_attention.apply(
        generation,
        Source::Doctor,
        Reply {
            success: false,
            stderr: "health check reported drift".into(),
            stdout: r#"{"drift":true}"#.into(),
        },
    );
    assert!(matches!(
        needs_attention.daemon(),
        DaemonState::NeedsAttention { .. }
    ));
    states.push(needs_attention);
    for (index, state) in states.iter().enumerate() {
        let color = ui::foreground(state);
        for background in [
            ui::BACKGROUND,
            Color::rgb(64, 64, 64),
            Color::rgb(55, 78, 100),
        ] {
            assert!(
                ui::contrast(color, background) >= 4.5,
                "fixture {index}: contrast {}",
                ui::contrast(color, background)
            );
        }
    }
}
#[test]
fn successful_start_waits_for_a_new_status_before_offering_another_start() {
    let mut state = fixture(2);
    assert!(state.begin(Action::Start));
    worker::finish(
        &mut state,
        Action::Start,
        worker::Outcome {
            success: true,
            message: "Soldr started.".into(),
            stdout: String::new(),
            stderr: String::new(),
            partial_restart: false,
        },
    );
    assert_eq!(state.daemon(), DaemonState::Starting);
    assert!(!state.can_submit(Action::Start));
    let generation = state.generation;
    assert!(!state.apply(
        generation - 1,
        Source::Daemon,
        reply(serde_json::json!({"running":false}))
    ));
    assert!(state.apply(
        generation,
        Source::Daemon,
        reply(serde_json::json!({"running":true}))
    ));
    assert_eq!(state.daemon(), DaemonState::Ready);
}

#[test]
fn pairing_expires_and_sharing_confirmation_has_a_deadline() {
    let mut state = fixture(1);
    state.pairing = Pairing::AwaitingPeer {
        code: "test-code".into(),
        since: state.now,
    };
    state.tick(state.now + 301);
    assert_eq!(state.pairing, Pairing::Expired);
    state.sharing.requested = Some(true);
    state.sharing.awaiting_refresh = true;
    state.sharing.requested_at = state.now;
    state.tick(state.now + 31);
    assert!(!state.sharing.displayed());
    assert!(matches!(state.action, ActionState::Failed { .. }));
}
#[test]
fn fixtures_and_dashboard_tabs_render_without_panic() {
    for index in 0..FIXTURES.len() {
        for mode in [Mode::Panel, Mode::Dashboard] {
            if !render(fixture(index), mode, Tab::Overview) {
                return;
            }
        }
    }
    for tab in [Tab::Builds, Tab::Nodes, Tab::Diagnostics] {
        if !render(fixture(1), Mode::Dashboard, tab) {
            return;
        }
    }
}
fn render(state: State, mode: Mode, tab: Tab) -> bool {
    let mut panel = Panel::fixture(state, mode);
    panel.tab = tab;
    match gfx::snapshot(
        &view::options(&panel.state, mode, false, panel.scale),
        &mut panel,
    ) {
        Ok(shot) => {
            assert_eq!(
                shot.rgba().len(),
                (shot.width() * shot.height() * 4) as usize
            );
            assert!(shot.width() >= 300);
            assert!(shot
                .rgba()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] > 100 && p[1] > 100));
            true
        }
        Err(error) => {
            let text = error.to_string();
            if [
                "No suitable graphics adapter",
                "NoAdapter",
                "No adapter",
                "no adapter",
                "No suitable adapter",
            ]
            .iter()
            .any(|s| text.contains(s))
            {
                eprintln!("SKIP screenshot smoke: no GPU/software rasteriser: {error}");
                return false;
            }
            panic!("snapshot failed: {error}");
        }
    }
}
