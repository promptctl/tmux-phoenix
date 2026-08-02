//! Tests for ticket `.5`: pane output routed separately from other
//! notifications, and the `on_notification`/`on_pane_output` sink API.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use tmux_control::{Client, PaneId, ServerMessage};

#[derive(Clone, Default)]
struct MockState {
    closed: Rc<RefCell<bool>>,
}

struct MockTransport {
    chunks: VecDeque<Vec<u8>>,
    state: MockState,
}

impl MockTransport {
    fn new(chunks: Vec<&str>) -> Self {
        Self {
            chunks: chunks.into_iter().map(|c| c.as_bytes().to_vec()).collect(),
            state: MockState::default(),
        }
    }
}

impl tmux_control::Transport for MockTransport {
    fn send(&mut self, command: &str) -> io::Result<()> {
        if *self.state.closed.borrow() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        let _ = command;
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.chunks.pop_front() {
            Some(chunk) => {
                assert!(
                    chunk.len() <= buf.len(),
                    "test chunk larger than read buffer"
                );
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
            None => Ok(0),
        }
    }

    fn close(&mut self) {
        *self.state.closed.borrow_mut() = true;
    }
}

#[test]
fn pane_output_is_buffered_separately_from_other_notifications() {
    let transport = MockTransport::new(vec![
        "%output %3 hello\\012\n",
        "%begin 1 1 1\nreply line\n%end 1 1 1\n",
    ]);
    let mut client = Client::new(transport);
    let result = client.execute("noop").unwrap();
    assert_eq!(result.lines, vec![b"reply line".to_vec()]);

    assert_eq!(client.drain_notifications(), vec![]);
    assert_eq!(
        client.drain_pane_output(),
        vec![(PaneId(3), b"hello\n".to_vec())]
    );
}

#[test]
fn extended_output_is_also_routed_as_pane_output() {
    let transport = MockTransport::new(vec![
        "%extended-output %2 500 : chunk\\012\n",
        "%begin 1 1 1\n%end 1 1 1\n",
    ]);
    let mut client = Client::new(transport);
    client.execute("noop").unwrap();

    assert_eq!(
        client.drain_pane_output(),
        vec![(PaneId(2), b"chunk\n".to_vec())]
    );
}

#[test]
fn non_output_notifications_still_go_through_drain_notifications() {
    let transport = MockTransport::new(vec![
        "%sessions-changed\n%window-add @1\n",
        "%begin 1 1 1\n%end 1 1 1\n",
    ]);
    let mut client = Client::new(transport);
    client.execute("noop").unwrap();

    assert_eq!(
        client.drain_notifications(),
        vec![
            ServerMessage::SessionsChanged,
            ServerMessage::WindowAdd {
                window: tmux_control::WindowId(1)
            },
        ]
    );
    assert_eq!(client.drain_pane_output(), vec![]);
}

#[test]
fn on_notification_sink_receives_messages_and_bypasses_the_buffer() {
    let received: Rc<RefCell<Vec<ServerMessage>>> = Rc::new(RefCell::new(Vec::new()));
    let received_in_sink = received.clone();

    let transport = MockTransport::new(vec!["%sessions-changed\n", "%begin 1 1 1\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    client.on_notification(move |msg| received_in_sink.borrow_mut().push(msg));

    client.execute("noop").unwrap();

    assert_eq!(*received.borrow(), vec![ServerMessage::SessionsChanged]);
    // The sink claimed it — nothing left in the fallback buffer.
    assert_eq!(client.drain_notifications(), vec![]);
}

#[test]
fn on_pane_output_sink_receives_bytes_and_bypasses_the_buffer() {
    let received = Rc::new(RefCell::new(Vec::<(PaneId, Vec<u8>)>::new()));
    let received_in_sink = received.clone();

    let transport = MockTransport::new(vec!["%output %5 hi\\012\n", "%begin 1 1 1\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    client.on_pane_output(move |pane, data| received_in_sink.borrow_mut().push((pane, data)));

    client.execute("noop").unwrap();

    assert_eq!(*received.borrow(), vec![(PaneId(5), b"hi\n".to_vec())]);
    assert_eq!(client.drain_pane_output(), vec![]);
}

#[test]
fn registering_a_sink_does_not_retroactively_deliver_already_buffered_messages() {
    let transport = MockTransport::new(vec![
        "%sessions-changed\n",
        "%begin 1 1 1\n%end 1 1 1\n",
        "%window-add @1\n",
        "%begin 2 2 1\n%end 2 2 1\n",
    ]);
    let mut client = Client::new(transport);

    // First command: no sink registered yet, notification lands in the buffer.
    client.execute("first").unwrap();
    assert_eq!(
        client.drain_notifications(),
        vec![ServerMessage::SessionsChanged]
    );

    // Now register a sink, then trigger the second notification.
    let received: Rc<RefCell<Vec<ServerMessage>>> = Rc::new(RefCell::new(Vec::new()));
    let received_in_sink = received.clone();
    client.on_notification(move |msg| received_in_sink.borrow_mut().push(msg));
    client.execute("second").unwrap();

    assert_eq!(
        *received.borrow(),
        vec![ServerMessage::WindowAdd {
            window: tmux_control::WindowId(1)
        }]
    );
    // Nothing left over in the buffer — the sink took the second one, and
    // the first was already drained above.
    assert_eq!(client.drain_notifications(), vec![]);
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

mod support;
use support::IsolatedTmux;
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

    let mut client = Client::connect(transport).expect("handshake failed against real tmux");

    // Send a real keystroke into the pane so tmux emits genuine %output —
    // unlike the other live tests, this one deliberately does NOT set
    // no-output, since the whole point here is to observe pane bytes.
    // No explicit -t: targets the attached session's active pane by
    // default, which is the only pane a freshly created session has.
    client
        .execute("send-keys 'echo phoenix-output-marker' Enter")
        .unwrap();

    // Poll a few times: pane output arrives as its own notification, not
    // necessarily bundled with the send-keys command's own (empty) reply.
    let mut found = false;
    for _ in 0..20 {
        let _ = client.execute("list-sessions"); // any command drives another read
        if client
            .drain_pane_output()
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
        client.drain_notifications().iter().all(|m| !matches!(
            m,
            ServerMessage::Output { .. } | ServerMessage::ExtendedOutput { .. }
        )),
        "pane output must never appear in the notification buffer"
    );

    client.close();
}
