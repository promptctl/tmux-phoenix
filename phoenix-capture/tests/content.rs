//! Live: `ContentCapture::On`'s dirty-tracking, proven end to end against a
//! real tmux server — not just that the types compile, but that an
//! unchanged pane's scrollback is actually *reused* (never re-captured)
//! and a changed pane's scrollback is actually *re-pulled*.

mod support;
use support::{line, IsolatedTmux};

use std::collections::HashMap;

use phoenix_capture::{capture, ContentCapture, PreviousPaneContent};
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

#[test]
fn unchanged_pane_reuses_previous_scrollback_and_changed_pane_re_captures() {
    let harness = IsolatedTmux::new("content-dirty-tracking");
    let mut client = connect(&harness);

    // First capture: no previous state at all, so the one pane is dirty by
    // definition (never seen before) and gets a real capture-pane pull.
    let first = capture(
        &mut client,
        ContentCapture::On {
            previous: HashMap::new(),
        },
    )
    .expect("first capture failed");
    let first_pane = first.sessions.first().active_window().active_pane();
    let first_content = first_pane
        .content
        .as_ref()
        .expect("content should be captured");
    let pane_id = first_pane.id.0;

    // Second capture: hand back an *injected* previous scrollback under the
    // same (history_size, history_bytes) the pane actually has right now —
    // real capture-pane output would never contain this literal marker, so
    // seeing it in the result proves reuse happened rather than a fresh pull.
    let mut previous = HashMap::new();
    previous.insert(
        pane_id,
        PreviousPaneContent {
            history_size: first_content.history_size,
            history_bytes: first_content.history_bytes,
            scrollback: vec!["INJECTED-PREVIOUS-MARKER".to_string()],
        },
    );
    let second = capture(
        &mut client,
        ContentCapture::On {
            previous: previous.clone(),
        },
    )
    .expect("second capture failed");
    let second_pane = second.sessions.first().active_window().active_pane();
    let second_content = second_pane.content.as_ref().unwrap();
    assert_eq!(
        second_content.scrollback,
        vec!["INJECTED-PREVIOUS-MARKER".to_string()],
        "an unchanged pane must reuse the previous scrollback verbatim, not re-capture"
    );

    // Now actually change the pane, and re-capture with the *same* stale
    // `previous` map (still claiming the old indicator) — the real
    // indicator has moved, so this pane must be treated as dirty and
    // re-captured for real, discarding the injected marker.
    client
        .execute(&line("send-keys", ["echo distinctive-new-output", "Enter"]))
        .unwrap();
    let mut third_content = None;
    for _ in 0..30 {
        let third = capture(
            &mut client,
            ContentCapture::On {
                previous: previous.clone(),
            },
        )
        .expect("third capture failed");
        let pane = third.sessions.first().active_window().active_pane();
        let content = pane.content.clone().unwrap();
        if content
            .scrollback
            .iter()
            .any(|line| line.contains("distinctive-new-output"))
        {
            third_content = Some(content);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let third_content =
        third_content.expect("changed pane's new output should appear in scrollback");
    assert!(
        !third_content
            .scrollback
            .iter()
            .any(|line| line.contains("INJECTED-PREVIOUS-MARKER")),
        "a changed pane must discard the stale injected marker, not reuse it"
    );

    client.close();
}

#[test]
fn content_capture_off_never_sets_pane_content() {
    let harness = IsolatedTmux::new("content-off");
    let mut client = connect(&harness);

    let snapshot = capture(&mut client, ContentCapture::Off).expect("capture failed");
    let pane = snapshot.sessions.first().active_window().active_pane();
    assert!(pane.content.is_none());

    client.close();
}

#[test]
fn content_capture_on_always_refreshes_the_visible_screen() {
    let harness = IsolatedTmux::new("content-visible-refresh");
    let mut client = connect(&harness);

    let first = capture(
        &mut client,
        ContentCapture::On {
            previous: HashMap::new(),
        },
    )
    .expect("first capture failed");
    let first_pane = first.sessions.first().active_window().active_pane();
    let first_content = first_pane.content.clone().unwrap();

    // Even with a matching indicator (scrollback reused), the visible
    // screen must be a *real* fresh capture, not carried over.
    let mut previous = HashMap::new();
    previous.insert(
        first_pane.id.0,
        PreviousPaneContent {
            history_size: first_content.history_size,
            history_bytes: first_content.history_bytes,
            scrollback: first_content.scrollback.clone(),
        },
    );
    let second =
        capture(&mut client, ContentCapture::On { previous }).expect("second capture failed");
    let second_content = second
        .sessions
        .first()
        .active_window()
        .active_pane()
        .content
        .clone()
        .unwrap();

    assert!(
        !second_content.visible.is_empty(),
        "the visible screen should always be captured, even for an unchanged pane"
    );

    client.close();
}
