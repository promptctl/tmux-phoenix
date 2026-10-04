//! Live: dirty-tracking proven end to end against a real tmux server — not
//! just that the types compile, but that an unchanged pane's scrollback is
//! actually *reused* (never re-captured) and a changed pane's scrollback is
//! actually *re-pulled*.

mod support;
use support::{line, IsolatedTmux};

use phoenix_capture::{capture, Previous, PreviousContent};
use phoenix_core::{Content, HistoryIndicator, Shells, Snapshot};
use tmux_control::{Client, SpawnOptions, SpawnTransport};

fn connect(harness: &IsolatedTmux) -> Client<SpawnTransport> {
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    Client::connect(transport, drop, |_, _| {}).expect("handshake failed against real tmux")
}

/// Blocks until the pane's dirty indicator holds still across consecutive
/// reads. A fresh shell is still drawing its prompt when the session
/// appears, and under load that output can land between two captures,
/// moving the indicator the reuse assertion depends on.
fn wait_for_quiet_pane(client: &mut Client<SpawnTransport>, target: &str) {
    let indicator = |client: &mut Client<SpawnTransport>| {
        client
            .execute(&line(
                "display-message",
                ["-p", "-t", target, "#{history_size} #{history_bytes}"],
            ))
            .expect("display-message failed")
            .lines
    };
    let mut last = indicator(client);
    let mut stable_reads = 0;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = indicator(client);
        stable_reads = if now == last { stable_reads + 1 } else { 0 };
        if stable_reads == 3 {
            return;
        }
        last = now;
    }
    panic!("pane {target} kept producing output for 5s");
}

/// The one pane's captured content and its id.
fn captured(
    snapshot: &Snapshot,
) -> (
    phoenix_core::PaneId,
    HistoryIndicator,
    Vec<String>,
    Vec<String>,
) {
    let session = snapshot.sessions().first();
    let pane = snapshot.active_window(session).active_pane();
    match &pane.content {
        Content::Captured {
            indicator,
            scrollback,
            visible,
        } => (pane.id, *indicator, scrollback.clone(), visible.clone()),
        Content::NotCaptured { reason } => panic!("content should be captured: {reason}"),
    }
}

#[test]
fn unchanged_pane_reuses_previous_scrollback_and_changed_pane_re_captures() {
    let harness = IsolatedTmux::new("content-dirty-tracking");
    let mut client = connect(&harness);
    wait_for_quiet_pane(&mut client, &harness.session);

    // First capture: no previous state at all, so the one pane is dirty by
    // definition (never seen before) and gets a real capture-pane pull.
    let first = capture(&mut client, &Previous::default(), &Shells::default())
        .expect("first capture failed");
    let (pane_id, indicator, _, _) = captured(&first);

    // Second capture: hand back an *injected* previous scrollback under the
    // same indicator the pane actually has right now — real capture-pane
    // output would never contain this literal marker, so seeing it in the
    // result proves reuse happened rather than a fresh pull.
    let mut previous = Previous::default();
    previous.insert(
        pane_id,
        PreviousContent {
            indicator,
            scrollback: vec!["INJECTED-PREVIOUS-MARKER".to_string()],
        },
    );
    let second =
        capture(&mut client, &previous, &Shells::default()).expect("second capture failed");
    let (_, _, second_scrollback, _) = captured(&second);
    assert_eq!(
        second_scrollback,
        vec!["INJECTED-PREVIOUS-MARKER".to_string()],
        "an unchanged pane must reuse the previous scrollback verbatim, not re-capture"
    );

    // Now actually change the pane, and re-capture with the *same* stale
    // `previous` (still claiming the old indicator) — the real indicator has
    // moved, so this pane must be treated as dirty and re-captured for real,
    // discarding the injected marker.
    client
        .execute(&line("send-keys", ["echo distinctive-new-output", "Enter"]))
        .unwrap();
    let mut third_scrollback = None;
    for _ in 0..30 {
        let third =
            capture(&mut client, &previous, &Shells::default()).expect("third capture failed");
        let (_, _, scrollback, _) = captured(&third);
        if scrollback
            .iter()
            .any(|line| line.contains("distinctive-new-output"))
        {
            third_scrollback = Some(scrollback);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let third_scrollback =
        third_scrollback.expect("changed pane's new output should appear in scrollback");
    assert!(
        !third_scrollback
            .iter()
            .any(|line| line.contains("INJECTED-PREVIOUS-MARKER")),
        "a changed pane must discard the stale injected marker, not reuse it"
    );

    client.close();
}

#[test]
fn the_visible_screen_is_always_refreshed_and_previous_comes_from_a_snapshot() {
    let harness = IsolatedTmux::new("content-visible-refresh");
    let mut client = connect(&harness);
    wait_for_quiet_pane(&mut client, &harness.session);

    let first = capture(&mut client, &Previous::default(), &Shells::default())
        .expect("first capture failed");
    let (_, _, first_scrollback, _) = captured(&first);

    // `Previous::from_snapshot` is the one bridge from a persisted snapshot:
    // with it, the unchanged pane reuses its scrollback, and the visible
    // screen must still be a *real* fresh capture, not carried over.
    let second = capture(
        &mut client,
        &Previous::from_snapshot(&first),
        &Shells::default(),
    )
    .expect("second capture failed");
    let (_, _, second_scrollback, second_visible) = captured(&second);
    assert_eq!(second_scrollback, first_scrollback);
    assert!(
        !second_visible.is_empty(),
        "the visible screen should always be captured, even for an unchanged pane"
    );

    client.close();
}
