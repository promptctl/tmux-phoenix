//! Tests for `Client::connect`/`reconnect`'s greeting handshake and the
//! `ConnectionState` gate `execute()` enforces. `client.rs` covers FIFO
//! correlation itself using `Client::new` (no handshake); this file is
//! entirely about the handshake and state machine ticket `.4` adds on top.

use tmux_control::{Client, CloseReason, ConnectionState, TmuxError};

const GREETING: &str = "%begin 1699900000 0 0\n%end 1699900000 0 0\n";

#[test]
fn connect_consumes_an_empty_greeting_and_becomes_ready() {
    let (transport, _state) = MockTransport::new(vec![GREETING]);
    let client = Client::connect(transport).unwrap();
    assert_eq!(client.state(), ConnectionState::Ready);
}

#[test]
fn connect_discards_greeting_output_without_buffering_it_as_a_notification() {
    let (transport, _state) = MockTransport::new(vec![
        "%begin 1699900000 0 0\nsome greeting-body line\n%end 1699900000 0 0\n",
    ]);
    let mut client = Client::connect(transport).unwrap();
    assert_eq!(client.drain_notifications(), vec![]);
}

#[test]
fn connect_buffers_a_real_notification_that_arrives_during_the_greeting_phase() {
    // Distinct from CommandOutput inside the greeting's own block (discarded
    // above): a genuine notification can arrive interleaved with, but
    // outside, the greeting's guard block.
    let (transport, _state) = MockTransport::new(vec![
        "%begin 1699900000 0 0\n%end 1699900000 0 0\n%sessions-changed\n",
    ]);
    let mut client = Client::connect(transport).unwrap();
    assert_eq!(
        client.drain_notifications(),
        vec![tmux_control::ServerMessage::SessionsChanged]
    );
}

#[test]
fn connect_treats_a_greeting_error_as_a_successful_handshake() {
    // Mirrors the reference client: a %error on the unsolicited greeting
    // itself isn't something a caller could retry or observe as a command
    // failure (there's no pending command). The phase still closes to Ready.
    let (transport, _state) =
        MockTransport::new(vec!["%begin 1699900000 0 0\n%error 1699900000 0 0\n"]);
    let client = Client::connect(transport).unwrap();
    assert_eq!(client.state(), ConnectionState::Ready);
}

#[test]
fn connect_treats_a_malformed_greeting_terminator_as_a_successful_handshake() {
    let (transport, _state) =
        MockTransport::new(vec!["%begin 1699900000 0 0\n%end 1699900000 0\n"]);
    let client = Client::connect(transport).unwrap();
    assert_eq!(client.state(), ConnectionState::Ready);
}

#[test]
fn connect_fails_and_closes_when_transport_closes_before_the_greeting_settles() {
    let (transport, _state) = MockTransport::new(vec!["%begin 1699900000 0 0\n"]); // no terminator, then EOF
    let err = match Client::connect(transport) {
        Err(err) => err,
        Ok(_) => panic!("expected connect() to fail before the greeting settled"),
    };
    assert!(matches!(err, TmuxError::TransportClosed));
}

#[test]
fn execute_before_connect_finishes_would_correlate_the_greeting_wrongly() {
    // This is the exact off-by-one DESIGN.md warns about, demonstrated on
    // Client::new (the escape hatch with no handshake) rather than by
    // finding a way to call execute() mid-connect() — connect() itself
    // can't return a not-yet-ready client, by construction.
    let (transport, _state) = MockTransport::new(vec![GREETING]);
    let mut client = Client::new(transport);
    // Without a handshake, the "greeting" block is indistinguishable from
    // a real reply and gets consumed as this command's own (empty) output.
    let result = client.execute(&line("some-command", NO_ARGS)).unwrap();
    assert_eq!(result.lines, Vec::<Vec<u8>>::new());
}

#[test]
fn execute_refuses_while_not_ready() {
    let (transport, _state) = MockTransport::new(vec![]);
    let mut client = Client::new(transport);
    // Force a non-Ready state the only way available post-construction.
    client.close();
    let err = client.execute(&line("anything", NO_ARGS)).unwrap_err();
    match err {
        TmuxError::NotReady(state) => {
            assert_eq!(
                state,
                ConnectionState::Closed {
                    reason: CloseReason::Disposed
                }
            );
        }
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[test]
fn execute_transitions_to_closed_on_transport_closed_eof() {
    let (transport, _state) = MockTransport::new(vec![]); // no chunks: immediate EOF
    let mut client = Client::new(transport);
    let err = client.execute(&line("anything", NO_ARGS)).unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed));
    assert_eq!(
        client.state(),
        ConnectionState::Closed {
            reason: CloseReason::Exit
        }
    );
}

#[test]
fn reconnect_swaps_transport_and_re_consumes_the_greeting() {
    let (first, _first_state) = MockTransport::new(vec![]); // dies with no greeting
    let mut client = Client::new(first); // starts Ready via new(), then dies below
    let _ = client.execute(&line("cmd-before-death", NO_ARGS));
    assert_eq!(
        client.state(),
        ConnectionState::Closed {
            reason: CloseReason::Exit
        }
    );

    let (second, second_state) = MockTransport::new(vec![GREETING]);
    client.reconnect(second, 1).unwrap();

    assert_eq!(client.state(), ConnectionState::Ready);
    assert_eq!(*second_state.sent.borrow(), Vec::<String>::new()); // handshake sends nothing
}

#[test]
fn reconnect_failure_reports_the_attempt_that_failed() {
    let (first, _first_state) = MockTransport::new(vec![]);
    let mut client = Client::new(first);
    let (second, _second_state) = MockTransport::new(vec![]); // also dies before any greeting
    let err = client.reconnect(second, 3).unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed));
    assert_eq!(
        client.state(),
        ConnectionState::Closed {
            reason: CloseReason::Exit
        }
    );
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

mod support;
use support::{line, IsolatedTmux, MockTransport, NO_ARGS};
use tmux_control::SpawnTransport;

#[test]
fn live_tmux_connect_handshakes_and_then_executes() {
    let harness = IsolatedTmux::new("connect-handshake");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &tmux_control::SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    // Unlike client.rs's live test, this drives the real handshake through
    // Client::connect() itself rather than draining the greeting by hand.
    let mut client = Client::connect(transport).expect("handshake failed against real tmux");
    assert_eq!(client.state(), ConnectionState::Ready);

    let result = client
        .execute(&line("display-message", ["-p", "PHOENIX-CONNECT-MARKER"]))
        .unwrap();
    let joined = result.lines.concat();
    let text = String::from_utf8_lossy(&joined);
    assert!(
        text.contains("PHOENIX-CONNECT-MARKER"),
        "unexpected output: {text}"
    );

    client.close();
    assert_eq!(
        client.state(),
        ConnectionState::Closed {
            reason: CloseReason::Disposed
        }
    );
}
