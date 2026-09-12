//! The M0 done-shape (ticket `tmux-control-mode-1ju.7`): parse *recorded*
//! tmux transcripts — real bytes captured from an actual `tmux -C` session,
//! not hand-written SPEC examples (ticket `.1`'s `codec.rs` already covers
//! those, plus every SPEC §23 message type in isolation) — into exact
//! `ServerMessage` values, and drive a live server through `execute` with
//! correlated replies across a broader, more realistic scenario than any
//! single earlier ticket's live test exercises alone.
//!
//! Fixtures under `fixtures/*.bin` were captured by spawning real
//! `tmux -C` on an isolated socket, driving it through a scripted sequence
//! of commands over stdin, and saving raw stdout. They contain genuine
//! terminal noise (a real zsh prompt's ANSI/256-color escape sequences, a
//! real `.zshrc` parse warning from this machine, real command numbers and
//! timestamps) — nothing here is synthesized. Structural assertions (which
//! notification arrived in which order, with which fields) are exact and
//! deterministic, since the fixture files are frozen; `%output`/command
//! output content is only spot-checked, since asserting every byte of a
//! captured shell prompt would make the test fragile to any of this
//! machine's shell config for no real benefit.

use std::fs;
use tmux_control::{Codec, Guard, PaneId, ServerMessage, SessionId, WindowId};

fn parse_fixture(name: &str) -> Vec<ServerMessage> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = fs::read(&path).unwrap_or_else(|e| panic!("failed to read fixture {path}: {e}"));
    let mut codec = Codec::new();
    let messages = codec.feed(&bytes);
    assert!(
        !messages.is_empty(),
        "fixture {name} produced no messages at all"
    );
    // The whole point of the codec's total/panic-free guarantee (DESIGN.md
    // §3.5) is that it never needs an Unknown/ProtocolError escape hatch
    // for bytes a real, protocol-compliant tmux actually sent. A real
    // capture landing in either means either a real parsing gap or (for
    // ProtocolError) a transcript that got truncated mid-block — worth
    // failing loudly on, not silently tolerating in a "conformance" suite.
    for msg in &messages {
        assert!(
            !matches!(
                msg,
                ServerMessage::Unknown(_) | ServerMessage::ProtocolError { .. }
            ),
            "fixture {name} produced {msg:?} — a real tmux transcript should never hit the \
             codec's malformed/unrecognized fallback"
        );
    }
    messages
}

/// Every message in `messages` except pane output and raw command-output
/// body lines — the "structural skeleton" of a session: guard framing plus
/// every typed notification, in the exact order tmux emitted them.
fn structural_skeleton(messages: &[ServerMessage]) -> Vec<ServerMessage> {
    messages
        .iter()
        .filter(|m| {
            !matches!(
                m,
                ServerMessage::Output { .. }
                    | ServerMessage::ExtendedOutput { .. }
                    | ServerMessage::CommandOutput { .. }
            )
        })
        .cloned()
        .collect()
}

fn guard(timestamp: i64, command_number: u32, flags: u32) -> Guard {
    Guard {
        timestamp,
        command_number,
        flags,
    }
}

#[test]
fn lifecycle_transcript_parses_into_the_exact_recorded_skeleton() {
    let messages = parse_fixture("lifecycle.bin");
    let skeleton = structural_skeleton(&messages);

    assert_eq!(
        skeleton,
        vec![
            // Unsolicited startup greeting.
            ServerMessage::GuardBegin(guard(1785677263, 554, 0)),
            ServerMessage::GuardEnd(guard(1785677263, 554, 0)),
            ServerMessage::SessionChanged {
                session: SessionId(0),
                name: "cap1".into()
            },
            // new-window -n second
            ServerMessage::GuardBegin(guard(1785677264, 561, 1)),
            ServerMessage::GuardEnd(guard(1785677264, 561, 1)),
            ServerMessage::SessionWindowChanged {
                session: SessionId(0),
                window: WindowId(1)
            },
            ServerMessage::WindowAdd {
                window: WindowId(1)
            },
            // rename-window -t second renamed
            ServerMessage::GuardBegin(guard(1785677264, 566, 1)),
            ServerMessage::GuardEnd(guard(1785677264, 566, 1)),
            ServerMessage::WindowRenamed {
                window: WindowId(1),
                name: "renamed".into()
            },
            // split-window -h -t renamed
            ServerMessage::GuardBegin(guard(1785677264, 568, 1)),
            ServerMessage::GuardEnd(guard(1785677264, 568, 1)),
            ServerMessage::WindowPaneChanged {
                window: WindowId(1),
                pane: PaneId(2)
            },
            ServerMessage::LayoutChange {
                window: WindowId(1),
                layout: tmux_control::Layout("020a,80x24,0,0{40x24,0,0,1,39x24,41,0,2}".into()),
                visible: tmux_control::Layout("020a,80x24,0,0{40x24,0,0,1,39x24,41,0,2}".into()),
                flags: "*".into(),
            },
            // select-window -t cap1:0 — this is the wrong target syntax and
            // fails for real; a genuine %error, not a synthesized one.
            ServerMessage::GuardBegin(guard(1785677265, 573, 1)),
            ServerMessage::GuardError(guard(1785677265, 573, 1)),
            // kill-window -t renamed
            ServerMessage::GuardBegin(guard(1785677265, 574, 1)),
            ServerMessage::GuardEnd(guard(1785677265, 574, 1)),
            ServerMessage::SessionWindowChanged {
                session: SessionId(0),
                window: WindowId(0)
            },
            ServerMessage::UnlinkedWindowClose {
                window: WindowId(1)
            },
            // The empty line we sent at the end triggers a clean detach.
            ServerMessage::Exit { reason: None },
        ]
    );
}

#[test]
fn lifecycle_transcript_error_reply_carries_the_real_tmux_error_text() {
    let messages = parse_fixture("lifecycle.bin");
    // Command 573's CommandOutput lines sit between its GuardBegin and
    // GuardError — the real "can't find window: 0" tmux sent for the
    // deliberately-wrong `select-window -t cap1:0` target syntax.
    let error_body: Vec<&[u8]> = messages
        .iter()
        .skip_while(|m| !matches!(m, ServerMessage::GuardBegin(g) if g.command_number == 573))
        .skip(1)
        .take_while(|m| !matches!(m, ServerMessage::GuardError(_)))
        .filter_map(|m| match m {
            ServerMessage::CommandOutput { line, .. } => Some(line.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(error_body, vec![b"can't find window: 0".as_slice()]);
}

#[test]
fn lifecycle_transcript_pane_output_decodes_readable_ansi_content() {
    let messages = parse_fixture("lifecycle.bin");
    let outputs: Vec<&[u8]> = messages
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Output { data, .. } => Some(data.as_slice()),
            _ => None,
        })
        .collect();
    assert!(
        !outputs.is_empty(),
        "expected real pane output in the capture"
    );
    // The captured shell prompt's window-title segment names this
    // directory's repo — decoded octal escapes must reproduce it literally,
    // proving decode_octal handles real, dense ANSI SGR sequences correctly
    // and not just SPEC's hand-picked examples.
    let all_output: Vec<u8> = outputs.concat();
    let text = String::from_utf8_lossy(&all_output);
    assert!(
        text.contains("tmux-phoenix"),
        "expected the shell prompt's repo name in decoded pane output"
    );
}

#[test]
fn subscription_and_buffers_transcript_parses_into_the_exact_recorded_skeleton() {
    let messages = parse_fixture("subscription_and_buffers.bin");
    let skeleton = structural_skeleton(&messages);

    assert_eq!(
        skeleton,
        vec![
            ServerMessage::GuardBegin(guard(1785677309, 554, 0)),
            ServerMessage::GuardEnd(guard(1785677309, 554, 0)),
            ServerMessage::SessionChanged {
                session: SessionId(0),
                name: "cap3".into()
            },
            // The unquoted `refresh-client -B mysub:%*:#{pane_dead}` — a
            // genuine, unforced demonstration of exactly the bug
            // `CommandLine` exists to prevent: `#{pane_dead}` reached tmux
            // unquoted, so `#` opened a comment and ate the rest of the line.
            ServerMessage::GuardBegin(guard(1785677309, 561, 1)),
            ServerMessage::GuardError(guard(1785677309, 561, 1)),
            ServerMessage::GuardBegin(guard(1785677310, 562, 1)),
            ServerMessage::GuardEnd(guard(1785677310, 562, 1)),
            ServerMessage::PasteBufferChanged {
                name: "buf0".into()
            },
            ServerMessage::GuardBegin(guard(1785677310, 564, 1)),
            ServerMessage::GuardEnd(guard(1785677310, 564, 1)),
            ServerMessage::PasteBufferDeleted {
                name: "buf0".into()
            },
            ServerMessage::PasteBufferChanged {
                name: "buf0".into()
            },
            ServerMessage::GuardBegin(guard(1785677310, 567, 1)),
            ServerMessage::GuardEnd(guard(1785677310, 567, 1)),
            ServerMessage::PasteBufferDeleted {
                name: "buf0".into()
            },
            ServerMessage::GuardBegin(guard(1785677310, 569, 1)),
            ServerMessage::GuardEnd(guard(1785677310, 569, 1)),
            ServerMessage::WindowPaneChanged {
                window: WindowId(0),
                pane: PaneId(1)
            },
            ServerMessage::LayoutChange {
                window: WindowId(0),
                layout: tmux_control::Layout("c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]".into()),
                visible: tmux_control::Layout("c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]".into()),
                flags: "*".into(),
            },
            ServerMessage::WindowRenamed {
                window: WindowId(0),
                name: "tmux".into()
            },
            // Automatic-rename firing again as the shell prompt redraws —
            // a real async notification unrelated to anything we sent.
            ServerMessage::WindowRenamed {
                window: WindowId(0),
                name: "zsh".into()
            },
            ServerMessage::GuardBegin(guard(1785677312, 576, 1)),
            ServerMessage::GuardEnd(guard(1785677312, 576, 1)),
            ServerMessage::Exit { reason: None },
        ]
    );
}

// ---------------------------------------------------------------------------
// Live round-trip: a broader scenario than any single earlier ticket's
// live test, exercising the crate end to end in one connected session.
// ---------------------------------------------------------------------------

mod support;
use support::{line, IsolatedTmux, NO_ARGS};
use tmux_control::commands::{
    query_tmux_version, set_no_output, subscribe, unsubscribe, SubscriptionName, SubscriptionScope,
};
use tmux_control::{Client, ConnectionState, SpawnOptions, SpawnTransport};

#[test]
fn live_round_trip_exercises_the_full_crate_against_a_real_server() {
    let harness = IsolatedTmux::new("full-round-trip");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    // Connect: real handshake, must reach Ready.
    let mut client = Client::connect(transport).expect("handshake failed against real tmux");
    assert_eq!(client.state(), ConnectionState::Ready);

    // Version gating: real probe must clear this crate's own floor.
    let version = query_tmux_version(&mut client).expect("version probe failed");
    assert!(version >= tmux_control::MIN_TMUX_VERSION);

    // Efficiency thesis (DESIGN.md §3.4): set no-output.
    set_no_output(&mut client).expect("set_no_output failed");

    // Subscriptions: subscribe, force a change, observe it, unsubscribe.
    // Parsed once and held (`[LAW:parse-dont-validate]`): the name tmux is
    // given, the name the match below watches for, and the name unsubscribed
    // are one binding, so a rename cannot silently stop matching.
    let sub_name =
        SubscriptionName::new("roundtrip-sub").expect("literal test name holds no colon");
    subscribe(
        &mut client,
        &sub_name,
        SubscriptionScope::AttachedSession,
        "#{session_windows}",
    )
    .expect("subscribe failed");
    client
        .execute(&line("new-window", NO_ARGS))
        .expect("new-window failed");

    let mut saw_subscription_changed = false;
    let mut saw_window_add = false;
    for _ in 0..30 {
        let _ = client.execute(&line("list-sessions", NO_ARGS));
        for msg in client.drain_notifications() {
            match msg {
                tmux_control::ServerMessage::SubscriptionChanged { ref name, .. }
                    if name == sub_name.as_str() =>
                {
                    saw_subscription_changed = true;
                }
                tmux_control::ServerMessage::WindowAdd { .. } => saw_window_add = true,
                _ => {}
            }
        }
        if saw_subscription_changed && saw_window_add {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        saw_window_add,
        "expected a WindowAdd notification for the new window"
    );
    assert!(
        saw_subscription_changed,
        "expected a SubscriptionChanged notification for {}",
        sub_name.as_str()
    );

    unsubscribe(&mut client, &sub_name).expect("unsubscribe failed");

    // A command that fails for real, correlated correctly even after
    // everything above.
    let err = client
        .execute(&line("this-is-not-a-real-command", NO_ARGS))
        .unwrap_err();
    assert!(matches!(err, tmux_control::TmuxError::Command { .. }));

    // The client must still be Ready after a command failure — a %error
    // reply settles the block same as %end; it doesn't kill the connection.
    assert_eq!(client.state(), ConnectionState::Ready);
    client
        .execute(&line("list-windows", NO_ARGS))
        .expect("client should still be usable after a command error");

    // no-output was set, but pane_output must remain empty regardless —
    // nothing routes there without a live pane actually producing bytes we
    // read while set, since with no-output active tmux never even sends
    // %output. Confirms the flag call actually reached tmux, not just that
    // the command succeeded syntactically.
    assert_eq!(client.drain_pane_output(), vec![]);

    // Detach then explicit close — both teardown paths (IMPL.md §2.4)
    // reachable from one still-Ready client.
    client.detach().expect("detach should succeed while Ready");
    client.close();
    assert!(matches!(client.state(), ConnectionState::Closed { .. }));
}
