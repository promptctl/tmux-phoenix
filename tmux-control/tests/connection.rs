//! `Connection` (ARCHITECTURE.md §4): an owned reader thread delivering
//! events to the caller's sink, `execute` correlating replies off the same stream,
//! and `open` reaching any server — with or without sessions — by trying
//! `attach-session` and reading its failure as the signal.
//!
//! Scripted tests stand a channel-backed link in for tmux so the contract
//! can be stated exactly; live tests drive a real `tmux -C` on an isolated
//! socket (never the user's default server) for the facts only tmux can
//! confirm.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tmux_control::commands::{split_window, subscribe, SubscriptionName, SubscriptionScope};
use tmux_control::{
    Attach, CloseReason, Connection, ConnectionState, Event, Opened, PaneId, ServerMessage,
    SessionName, Target, TmuxError,
};

mod support;
use support::{line, scripted, sessions_on, EmptySocket, IsolatedTmux, Script, NO_ARGS};

const GREETING: &str = "%begin 1 0 0\n%end 1 0 0\n";
const NO_SESSIONS_GREETING: &str = "%begin 1 0 0\nno sessions\n%error 1 0 0\n%exit\n";

/// The caller's side of an event sink: what the daemon will build for
/// itself, here so a test can assert over what the reader delivered.
fn channel_sink() -> (impl FnMut(Event) + Send + 'static, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    (
        move |event| {
            let _ = tx.send(event);
        },
        rx,
    )
}

/// A connection past its greeting, the script that plays tmux, and the
/// events the reader delivered.
fn connected() -> (Connection, Script, support::Sent, Receiver<Event>) {
    let (writer, reader, script, sent) = scripted(vec![GREETING]);
    let (sink, events) = channel_sink();
    let connection = Connection::over(writer, reader, sink).expect("greeting settles");
    (connection, script, sent, events)
}

/// A greeting-only `over`, for the tests about what the greeting means.
fn over_scripted(chunks: Vec<&str>) -> Result<Connection, TmuxError> {
    let (writer, reader, _script, _sent) = scripted(chunks);
    Connection::over(writer, reader, drop::<Event>)
}

fn soon() -> Duration {
    Duration::from_secs(5)
}

fn next(events: &Receiver<Event>) -> Event {
    events
        .recv_timeout(soon())
        .expect("an event within the deadline")
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

#[test]
fn over_consumes_the_greeting_and_sends_nothing_for_it() {
    let (connection, _script, sent, _events) = connected();
    assert_eq!(connection.state(), ConnectionState::Ready);
    assert_eq!(sent.lines(), Vec::<String>::new());
}

#[test]
fn a_no_sessions_greeting_is_the_typed_error() {
    let err = over_scripted(vec![NO_SESSIONS_GREETING]).unwrap_err();
    assert!(matches!(err, TmuxError::NoSessions), "got {err:?}");
}

#[test]
fn a_failed_greeting_delivers_nothing_to_the_sink() {
    // The `%exit` behind a refused attach belongs to a process that never
    // became the caller's connection; a daemon counting `Closed`s must not
    // see one here.
    let (writer, reader, _script, _sent) = scripted(vec![NO_SESSIONS_GREETING]);
    let (sink, events) = channel_sink();
    let err = Connection::over(writer, reader, sink).unwrap_err();
    assert!(matches!(err, TmuxError::NoSessions), "got {err:?}");
    assert_eq!(
        events.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn any_other_greeting_error_is_the_command_failure_it_is() {
    // `new-session -s x` on a server that already has an `x`: tmux reports
    // it and exits. Reading it as "no sessions" would create a second scratch
    // session; skipping past it (as `Client::connect` does) would hand back a
    // connection to a process that has already gone.
    let err = over_scripted(vec![
        "%begin 1 0 0\nduplicate session: x\n%error 1 0 0\n%exit\n",
    ])
    .unwrap_err();
    match err {
        TmuxError::Command { lines, .. } => {
            assert_eq!(lines, vec![b"duplicate session: x".to_vec()])
        }
        other => panic!("expected TmuxError::Command, got {other:?}"),
    }
}

#[test]
fn eof_before_the_greeting_is_transport_closed() {
    let (writer, reader, script, _sent) = scripted(vec![]);
    script.hangup();
    let err = Connection::over(writer, reader, drop::<Event>).unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

#[test]
fn execute_sends_the_command_and_returns_the_block_that_follows() {
    let (mut connection, script, sent, _events) = connected();
    script.send("%begin 1 1 1\n0: bash* (1 panes)\n%end 1 1 1\n");

    let output = connection.execute(&line("list-windows", NO_ARGS)).unwrap();

    assert_eq!(output.guard.command_number, 1);
    assert_eq!(output.lines, vec![b"0: bash* (1 panes)".to_vec()]);
    assert_eq!(sent.lines(), vec!["list-windows".to_owned()]);
}

#[test]
fn an_error_reply_is_a_command_error_carrying_its_output() {
    let (mut connection, script, _sent, _events) = connected();
    script.send("%begin 1 3 1\nparse error\n%error 1 3 1\n");

    let err = connection
        .execute(&line("bad-command", NO_ARGS))
        .unwrap_err();
    match err {
        TmuxError::Command { guard, lines } => {
            assert_eq!(guard.command_number, 3);
            assert_eq!(lines, vec![b"parse error".to_vec()]);
        }
        other => panic!("expected TmuxError::Command, got {other:?}"),
    }
    assert_eq!(connection.state(), ConnectionState::Ready);
}

#[test]
fn a_reply_split_across_reads_is_assembled_whole() {
    let (mut connection, script, _sent, _events) = connected();
    script.send("%begin 1 5 1\n");
    script.send("line one\n");
    script.send("line two\n%end 1 5 1\n");

    let output = connection.execute(&line("list-panes", NO_ARGS)).unwrap();
    assert_eq!(
        output.lines,
        vec![b"line one".to_vec(), b"line two".to_vec()]
    );
}

#[test]
fn a_notification_ahead_of_the_reply_goes_to_the_sink_not_the_caller() {
    let (mut connection, script, _sent, events) = connected();
    script.send("%sessions-changed\n%begin 1 7 1\nok\n%end 1 7 1\n");

    let output = connection.execute(&line("list-windows", NO_ARGS)).unwrap();
    assert_eq!(output.lines, vec![b"ok".to_vec()]);
    assert_eq!(
        next(&events),
        Event::Notification(ServerMessage::SessionsChanged)
    );
}

#[test]
fn a_block_that_settles_with_nothing_in_flight_is_a_protocol_failure() {
    // tmux answers one block per command; a second one here would become
    // the "reply" to the next command and shift every reply after it.
    let (mut connection, script, _sent, _events) = connected();
    script.send("%begin 1 7 1\nok\n%end 1 7 1\n%begin 1 8 1\nstray\n%end 1 8 1\n");

    let output = connection.execute(&line("list-windows", NO_ARGS)).unwrap();
    assert_eq!(output.lines, vec![b"ok".to_vec()]);
    let err = connection
        .execute(&line("list-panes", NO_ARGS))
        .unwrap_err();
    match err {
        TmuxError::UnsolicitedReply(reply) => {
            assert_eq!(reply.unwrap().lines, vec![b"stray".to_vec()])
        }
        other => panic!("expected TmuxError::UnsolicitedReply, got {other:?}"),
    }
}

#[test]
fn execute_after_eof_reports_the_ending_and_closes() {
    let (mut connection, script, _sent, _events) = connected();
    script.hangup();

    let err = connection.execute(&line("anything", NO_ARGS)).unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed), "got {err:?}");
    assert_eq!(
        connection.state(),
        ConnectionState::Closed {
            reason: CloseReason::Exit
        }
    );
    // Closed is terminal: the next call refuses rather than blocking on a
    // reader that is gone.
    assert!(matches!(
        connection.execute(&line("anything", NO_ARGS)),
        Err(TmuxError::NotReady(ConnectionState::Closed { .. }))
    ));
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[test]
fn a_notification_reaches_the_sink_while_nothing_is_in_flight() {
    let (_connection, script, sent, events) = connected();
    script.send("%sessions-changed\n");

    assert_eq!(
        next(&events),
        Event::Notification(ServerMessage::SessionsChanged)
    );
    // Nothing was sent to provoke it: no heartbeat exists.
    assert_eq!(sent.lines(), Vec::<String>::new());
}

#[test]
fn pane_output_is_its_own_event() {
    let (_connection, script, _sent, events) = connected();
    script.send("%output %3 hi\\015\\012\n");

    assert_eq!(
        next(&events),
        Event::PaneOutput(PaneId(3), b"hi\r\n".to_vec())
    );
}

#[test]
fn events_behind_the_greeting_terminator_reach_the_sink() {
    // Whatever tmux wrote in the same read as the greeting's `%end` was
    // decoded before the sink existed; it is delivered, not dropped.
    let (writer, reader, _script, _sent) =
        scripted(vec!["%begin 1 0 0\n%end 1 0 0\n%sessions-changed\n"]);
    let (sink, events) = channel_sink();
    let _connection = Connection::over(writer, reader, sink).expect("greeting settles");
    assert_eq!(
        next(&events),
        Event::Notification(ServerMessage::SessionsChanged)
    );
}

#[test]
fn eof_is_delivered_last_as_closed_and_the_next_call_reports_it() {
    let (mut connection, script, _sent, events) = connected();
    script.send("%exit\n");
    script.hangup();

    assert_eq!(
        next(&events),
        Event::Notification(ServerMessage::Exit { reason: None })
    );
    assert_eq!(next(&events), Event::Closed(CloseReason::Exit));
    assert_eq!(
        events.recv_timeout(soon()),
        Err(RecvTimeoutError::Disconnected),
        "Closed is the reader's last word"
    );
    // The sink saw the ending; the connection reports it — as the clean
    // exit it was, not as a broken pipe — on the next call, then refuses.
    let err = connection.execute(&line("anything", NO_ARGS)).unwrap_err();
    assert!(matches!(err, TmuxError::TransportClosed), "got {err:?}");
    assert_eq!(
        connection.state(),
        ConnectionState::Closed {
            reason: CloseReason::Exit
        }
    );
    assert!(matches!(
        connection.execute(&line("anything", NO_ARGS)),
        Err(TmuxError::NotReady(ConnectionState::Closed { .. }))
    ));
}

#[test]
fn close_ends_the_reader_and_reports_disposed() {
    let (mut connection, _script, _sent, _events) = connected();
    // The reader is blocked in read() with nothing fed. close() must return
    // anyway — the writer's drop is what hangs the output half up — and this
    // test finishing is the proof that the join did not wait forever.
    connection.close();
    assert_eq!(
        connection.state(),
        ConnectionState::Closed {
            reason: CloseReason::Disposed
        }
    );
    connection.close();
    assert_eq!(
        connection.state(),
        ConnectionState::Closed {
            reason: CloseReason::Disposed
        }
    );
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

fn session_name(raw: &str) -> SessionName {
    SessionName::parse(raw).expect("test session names are addressable")
}

/// `display-message -p <format>` over the connection, as one string.
fn display(connection: &mut Connection, format: &str) -> String {
    let output = connection
        .execute(&line("display-message", ["-p", format]))
        .expect("display-message");
    String::from_utf8_lossy(&output.lines.concat()).into_owned()
}

#[test]
fn live_existing_on_no_server_is_no_sessions_and_leaves_no_server_behind() {
    let socket = EmptySocket::new("open-existing-empty");

    let err = Connection::open(&socket.options(), Attach::Existing, drop::<Event>).unwrap_err();

    assert!(matches!(err, TmuxError::NoSessions), "got {err:?}");
    assert_eq!(sessions_on(&socket.socket), Vec::<String>::new());
}

#[test]
fn live_or_create_on_no_server_creates_the_named_session_and_attaches_to_it() {
    let socket = EmptySocket::new("open-or-create-empty");
    let name = session_name("phoenix-scratch");

    let (mut connection, opened) = Connection::open(
        &socket.options(),
        Attach::OrCreate { name: name.clone() },
        drop::<Event>,
    )
    .expect("open");

    assert_eq!(opened, Opened::Created(name.clone()));
    assert_eq!(connection.state(), ConnectionState::Ready);
    assert_eq!(display(&mut connection, "#{session_name}"), name.as_str());
    assert_eq!(sessions_on(&socket.socket), vec![name.as_str().to_owned()]);
}

#[test]
fn live_or_create_on_a_populated_server_attaches_and_creates_nothing() {
    let harness = IsolatedTmux::new("open-or-create-populated");

    let (mut connection, opened) = Connection::open(
        &harness.options(),
        Attach::OrCreate {
            name: session_name("never-made"),
        },
        drop::<Event>,
    )
    .expect("open");

    assert_eq!(opened, Opened::Attached);
    assert_eq!(display(&mut connection, "#{session_name}"), harness.session);
    assert_eq!(sessions_on(&harness.socket), vec![harness.session.clone()]);
}

#[test]
fn live_a_subscription_fires_on_the_sink_while_idle() {
    let harness = IsolatedTmux::new("subscription-idle");
    let (sink, events) = channel_sink();
    let (mut connection, _) =
        Connection::open(&harness.options(), Attach::Existing, sink).expect("open");
    let name = SubscriptionName::new("windows").unwrap();
    subscribe(
        &mut connection,
        &name,
        SubscriptionScope::AttachedSession,
        "#{session_windows}",
    )
    .expect("subscribe");

    // Out of band, as the user would: a second window.
    let status = std::process::Command::new("tmux")
        .args([
            "-S",
            &harness.socket,
            "new-window",
            "-d",
            "-t",
            &Target::Session(session_name(&harness.session)).to_string(),
        ])
        .status()
        .expect("tmux new-window");
    assert!(status.success());

    // Nothing is executed from here on; the reader alone has to deliver it.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Notification(ServerMessage::SubscriptionChanged {
                name: changed,
                value,
                ..
            })) if changed == name.as_str() && value == "2" => break,
            Ok(_) => continue,
            Err(err) => panic!("no %subscription-changed for the new window arrived: {err}"),
        }
    }
}

#[test]
fn live_split_window_returns_the_id_of_the_pane_it_made() {
    let harness = IsolatedTmux::new("split-id");
    let mut connection = harness.connect();
    let first = display(&mut connection, "#{pane_id}");
    let first = PaneId::parse(first.as_bytes()).expect("a pane id");

    let created = split_window(&mut connection, first, Some("/tmp")).expect("split-window");

    assert_ne!(created, first);
    // `list-panes -t <pane>` lists that pane's window, so this says both
    // that the id exists and that it was split out of `first`'s window.
    let output = connection
        .execute(&line(
            "list-panes",
            ["-t", &Target::Pane(created).to_string(), "-F", "#{pane_id}"],
        ))
        .expect("list-panes");
    assert_eq!(
        output.lines,
        vec![
            format!("%{}", first.0).into_bytes(),
            format!("%{}", created.0).into_bytes()
        ]
    );
}
