//! Tests for ticket `.5`: pane output routed to its own sink, separately
//! from every other notification.
//!
//! Both sinks are constructor arguments, so there is no "was a sink
//! registered yet" dimension left to test — what remains is the routing
//! itself, which is the contract that always mattered.

use tmux_control::{PaneId, ServerMessage};

#[test]
fn pane_output_is_routed_separately_from_other_notifications() {
    let (transport, _state) = MockTransport::new(vec![
        "%output %3 hello\\012\n",
        "%begin 1 1 1\nreply line\n%end 1 1 1\n",
    ]);
    let (mut client, collected) = collecting_client(transport);
    let result = client.execute(&line("noop", NO_ARGS)).unwrap();
    assert_eq!(result.lines, vec![b"reply line".to_vec()]);

    assert_eq!(collected.take_notifications(), vec![]);
    assert_eq!(
        collected.take_pane_output(),
        vec![(PaneId(3), b"hello\n".to_vec())]
    );
}

#[test]
fn extended_output_is_also_routed_as_pane_output() {
    let (transport, _state) = MockTransport::new(vec![
        "%extended-output %2 500 : chunk\\012\n",
        "%begin 1 1 1\n%end 1 1 1\n",
    ]);
    let (mut client, collected) = collecting_client(transport);
    client.execute(&line("noop", NO_ARGS)).unwrap();

    assert_eq!(
        collected.take_pane_output(),
        vec![(PaneId(2), b"chunk\n".to_vec())]
    );
    assert_eq!(collected.take_notifications(), vec![]);
}

#[test]
fn non_output_notifications_go_to_the_notification_sink() {
    let (transport, _state) = MockTransport::new(vec![
        "%sessions-changed\n%window-add @1\n",
        "%begin 1 1 1\n%end 1 1 1\n",
    ]);
    let (mut client, collected) = collecting_client(transport);
    client.execute(&line("noop", NO_ARGS)).unwrap();

    assert_eq!(
        collected.take_notifications(),
        vec![
            ServerMessage::SessionsChanged,
            ServerMessage::WindowAdd {
                window: tmux_control::WindowId(1)
            },
        ]
    );
    assert_eq!(collected.take_pane_output(), vec![]);
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

mod support;
use support::{collecting_client, collecting_connect, line, IsolatedTmux, MockTransport, NO_ARGS};
use tmux_control::SpawnTransport;

#[test]
fn live_tmux_pane_output_arrives_through_the_pane_output_path() {
    let harness = IsolatedTmux::new("pane-output");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &tmux_control::SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");

    let (mut client, collected) =
        collecting_connect(transport).expect("handshake failed against real tmux");

    // Send a real keystroke into the pane so tmux emits genuine %output —
    // unlike the other live tests, this one deliberately does NOT set
    // no-output, since the whole point here is to observe pane bytes.
    // No explicit -t: targets the attached session's active pane by
    // default, which is the only pane a freshly created session has.
    client
        .execute(&line("send-keys", ["echo phoenix-output-marker", "Enter"]))
        .unwrap();

    // Poll a few times: pane output arrives as its own notification, not
    // necessarily bundled with the send-keys command's own (empty) reply.
    let mut found = false;
    for _ in 0..20 {
        let _ = client.execute(&line("list-sessions", NO_ARGS)); // any command drives another read
        if collected
            .take_pane_output()
            .iter()
            .any(|(_, data)| String::from_utf8_lossy(data).contains("phoenix-output-marker"))
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "expected to observe our echoed marker in pane output"
    );
    assert!(
        collected.take_notifications().iter().all(|m| !matches!(
            m,
            ServerMessage::Output { .. } | ServerMessage::ExtendedOutput { .. }
        )),
        "pane output must never reach the notification sink"
    );

    client.close();
}
