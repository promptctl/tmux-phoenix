//! The pane-creating commands and the typed targets they address, scripted
//! for the exact command lines and the reply parsing, live for what tmux
//! actually does with them.

use tmux_control::commands::{
    move_window, new_session, new_window, split_window, switch_client, Moved, NewSession,
    NewWindow, Switched,
};
use tmux_control::{PaneId, SessionId, SessionName, Target, TmuxError, WindowId, WindowIndex};

mod support;
use support::{collecting_client, line, IsolatedTmux, MockTransport};

fn name(raw: &str) -> SessionName {
    SessionName::parse(raw).expect("addressable")
}

fn reply(body: &str) -> String {
    format!("%begin 1 1 1\n{body}\n%end 1 1 1\n")
}

// ---------------------------------------------------------------------------
// Command lines and reply parsing
// ---------------------------------------------------------------------------

#[test]
fn new_session_is_detached_and_asks_for_its_ids() {
    let (transport, state) = MockTransport::new(vec![&reply("$2 @4 %5")]);
    let (mut client, _collected) = collecting_client(transport);

    let created = new_session(&mut client, &name("zz"), Some("first"), Some("/tmp")).unwrap();

    assert_eq!(
        created,
        NewSession {
            session: SessionId(2),
            window: WindowId(4),
            pane: PaneId(5),
        }
    );
    assert_eq!(
        *state.sent.borrow(),
        vec![r##"new-session -d -s zz -n first -c /tmp -P -F "#{session_id} #{window_id} #{pane_id}""##.to_owned()]
    );
}

#[test]
fn optional_flags_are_absent_when_not_given() {
    let (transport, state) = MockTransport::new(vec![&reply("$0 @0 %0")]);
    let (mut client, _collected) = collecting_client(transport);

    new_session(&mut client, &name("bare"), None, None).unwrap();

    assert_eq!(
        *state.sent.borrow(),
        vec![
            r##"new-session -d -s bare -P -F "#{session_id} #{window_id} #{pane_id}""##.to_owned()
        ]
    );
}

#[test]
fn new_window_targets_the_exact_session_and_index() {
    let (transport, state) = MockTransport::new(vec![&reply("@2 %2")]);
    let (mut client, _collected) = collecting_client(transport);

    let created = new_window(&mut client, &name("ab"), WindowIndex(5), Some("five"), None).unwrap();

    assert_eq!(
        created,
        NewWindow {
            window: WindowId(2),
            pane: PaneId(2),
        }
    );
    assert_eq!(
        *state.sent.borrow(),
        vec![r##"new-window -d -t =ab:=5 -n five -P -F "#{window_id} #{pane_id}""##.to_owned()]
    );
}

#[test]
fn split_window_targets_the_pane_by_id() {
    let (transport, state) = MockTransport::new(vec![&reply("%4")]);
    let (mut client, _collected) = collecting_client(transport);

    let created = split_window(&mut client, PaneId(0), Some("/tmp")).unwrap();

    assert_eq!(created, PaneId(4));
    assert_eq!(
        *state.sent.borrow(),
        // `%` is special to tmux's lexer, so the encoder quotes the pane id.
        vec![r##"split-window -d -t "%0" -c /tmp -P -F "#{pane_id}""##.to_owned()]
    );
}

#[test]
fn a_reply_without_the_asked_for_ids_is_unexpected_not_a_pane() {
    for body in ["", "%4 extra", "not-an-id", "@4"] {
        let (transport, _state) = MockTransport::new(vec![&reply(body)]);
        let (mut client, _collected) = collecting_client(transport);
        let err = split_window(&mut client, PaneId(0), None).unwrap_err();
        match err {
            TmuxError::UnexpectedReply { expected, output } => {
                assert_eq!(expected, "#{pane_id}");
                // The whole reply travels, whichever field was at fault.
                assert_eq!(output, vec![body.as_bytes().to_vec()], "{body:?}");
            }
            other => panic!("{body:?} gave {other:?}"),
        }
    }

    // A reply whose shape is right but whose fields are not: the full line,
    // not the one field, is what the operator gets to look at.
    let (transport, _state) = MockTransport::new(vec![&reply("$2 @4 x5")]);
    let (mut client, _collected) = collecting_client(transport);
    let err = new_session(&mut client, &name("zz"), None, None).unwrap_err();
    match err {
        TmuxError::UnexpectedReply { output, .. } => {
            assert_eq!(output, vec![b"$2 @4 x5".to_vec()])
        }
        other => panic!("expected UnexpectedReply, got {other:?}"),
    }
}

#[test]
fn move_window_names_the_window_by_id_and_reads_same_index_as_already_there() {
    let (transport, state) = MockTransport::new(vec![
        "%begin 1 1 1\n%end 1 1 1\n",
        "%begin 2 2 1\nsame index: 3\n%error 2 2 1\n",
        "%begin 3 3 1\nsame index: 4\n%error 3 3 1\n",
    ]);
    let (mut client, _collected) = collecting_client(transport);
    let mut moved = || move_window(&mut client, WindowId(9), &name("zz"), WindowIndex(3));

    assert_eq!(moved().unwrap(), Moved::Moved);
    assert_eq!(moved().unwrap(), Moved::AlreadyThere);
    // Only the refusal naming the index asked for is the answer "it is
    // there"; any other error stays an error.
    assert!(matches!(moved(), Err(TmuxError::Command { .. })));
    assert_eq!(
        state.sent.borrow()[0],
        "move-window -d -s @9 -t =zz:=3".to_owned()
    );
}

#[test]
fn switch_client_reads_a_client_that_detached_as_gone() {
    let (transport, state) = MockTransport::new(vec![
        "%begin 1 1 1\n%end 1 1 1\n",
        "%begin 2 2 1\ncan't find client: /dev/ttys004\n%error 2 2 1\n",
        "%begin 3 3 1\ncan't find session: zz\n%error 3 3 1\n",
    ]);
    let (mut client, _collected) = collecting_client(transport);
    let mut switched = || switch_client(&mut client, "/dev/ttys004", &name("zz"));

    assert_eq!(switched().unwrap(), Switched::Switched);
    assert_eq!(switched().unwrap(), Switched::Gone);
    // Only the client being gone is an answer; a missing session is an error.
    assert!(matches!(switched(), Err(TmuxError::Command { .. })));
    assert_eq!(
        state.sent.borrow()[0],
        "switch-client -c /dev/ttys004 -t =zz:".to_owned()
    );
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

#[test]
fn live_move_window_places_a_new_sessions_window_and_says_when_it_was_there() {
    let harness = IsolatedTmux::new("move-window");
    let mut connection = harness.connect();
    let session = name(&format!("{}-moved", harness.session));
    let created = new_session(&mut connection, &session, None, None).expect("new-session");
    let index_of_the_window = |connection: &mut tmux_control::Connection| {
        list_panes(
            connection,
            &Target::WindowId(created.window),
            "#{window_index}",
        )
    };

    let first = move_window(&mut connection, created.window, &session, WindowIndex(6));
    assert_eq!(first.expect("move-window"), Moved::Moved);
    assert_eq!(index_of_the_window(&mut connection), ["6"]);

    let again = move_window(&mut connection, created.window, &session, WindowIndex(6));
    assert_eq!(again.expect("move-window"), Moved::AlreadyThere);
    assert_eq!(index_of_the_window(&mut connection), ["6"]);

    connection
        .execute(&line(
            "kill-session",
            ["-t", &Target::Session(session).to_string()],
        ))
        .expect("kill-session");
}

/// `list-panes -t <target> -F <format>`, one string per line.
fn list_panes(
    connection: &mut tmux_control::Connection,
    target: &Target,
    format: &str,
) -> Vec<String> {
    let output = connection
        .execute(&line(
            "list-panes",
            ["-t", &target.to_string(), "-F", format],
        ))
        .unwrap_or_else(|e| panic!("list-panes -t {target}: {e}"));
    output
        .lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

#[test]
fn live_new_window_lands_at_the_index_asked_for_and_reports_its_ids() {
    let harness = IsolatedTmux::new("new-window-index");
    let mut connection = harness.connect();
    let session = name(&harness.session);

    let created = new_window(
        &mut connection,
        &session,
        WindowIndex(7),
        Some("seven"),
        Some("/tmp"),
    )
    .expect("new-window");

    let target = Target::Window(session.clone(), WindowIndex(7));
    assert_eq!(
        list_panes(
            &mut connection,
            &target,
            "#{window_id} #{pane_id} #{window_name} #{pane_current_path}"
        ),
        vec![format!(
            "@{} %{} seven /private/tmp",
            created.window.0, created.pane.0
        )]
        .into_iter()
        .map(|s| s.replace(
            "/private/tmp",
            &std::fs::canonicalize("/tmp").unwrap().display().to_string()
        ))
        .collect::<Vec<_>>()
    );

    // The same index again is tmux's refusal, carried as the command error
    // it is — not a silent relocation.
    let err = new_window(&mut connection, &session, WindowIndex(7), None, None).unwrap_err();
    match err {
        TmuxError::Command { lines, .. } => {
            assert_eq!(
                lines,
                vec![b"create window failed: index 7 in use".to_vec()]
            )
        }
        other => panic!("expected TmuxError::Command, got {other:?}"),
    }
}

#[test]
fn live_new_session_reports_the_ids_of_what_it_made() {
    let harness = IsolatedTmux::new("new-session-ids");
    let mut connection = harness.connect();
    let name = name(&format!("{}-second", harness.session));

    let created = new_session(&mut connection, &name, Some("first"), None).expect("new-session");

    assert_eq!(
        list_panes(
            &mut connection,
            &Target::Session(name.clone()),
            "#{session_id} #{window_id} #{pane_id} #{window_name}"
        ),
        vec![format!(
            "${} @{} %{} first",
            created.session.0, created.window.0, created.pane.0
        )]
    );
    connection
        .execute(&line(
            "kill-session",
            ["-t", &Target::Session(name).to_string()],
        ))
        .expect("kill-session");
}

#[test]
fn live_an_exact_session_target_does_not_match_a_longer_name() {
    let harness = IsolatedTmux::new("exact-target");
    let mut connection = harness.connect();
    let longer = name(&format!("{}-x", harness.session));
    new_session(&mut connection, &longer, None, None).expect("new-session");

    let exact = Target::Session(name(&harness.session));
    assert_eq!(
        list_panes(&mut connection, &exact, "#{session_name}"),
        vec![harness.session.clone()]
    );

    connection
        .execute(&line(
            "kill-session",
            ["-t", &Target::Session(longer).to_string()],
        ))
        .expect("kill-session");
}

#[test]
fn live_a_dotted_session_name_is_addressed_exactly() {
    // `.` is tmux's window/pane separator, so `=a.b` alone cannot name this
    // session; the trailing colon the renderer adds is what makes it exact.
    let harness = IsolatedTmux::new("dotted-name");
    let mut connection = harness.connect();
    let dotted = name(&format!("{}.v2", harness.session));
    let created = new_session(&mut connection, &dotted, None, None).expect("new-session");

    assert_eq!(
        list_panes(
            &mut connection,
            &Target::Session(dotted.clone()),
            "#{session_name}"
        ),
        vec![dotted.as_str().to_owned()]
    );
    assert_eq!(
        list_panes(
            &mut connection,
            &Target::SessionId(created.session),
            "#{session_name}"
        ),
        vec![dotted.as_str().to_owned()]
    );
    assert_eq!(
        list_panes(
            &mut connection,
            &Target::WindowId(created.window),
            "#{pane_id}"
        ),
        vec![format!("%{}", created.pane.0)]
    );
    connection
        .execute(&line(
            "kill-session",
            ["-t", &Target::Session(dotted).to_string()],
        ))
        .expect("kill-session");
}

#[test]
fn live_a_missing_exact_target_is_a_command_error() {
    let harness = IsolatedTmux::new("missing-target");
    let mut connection = harness.connect();

    let err = connection
        .execute(&line(
            "list-panes",
            ["-t", &Target::Session(name("no-such-session")).to_string()],
        ))
        .unwrap_err();
    assert!(matches!(err, TmuxError::Command { .. }), "got {err:?}");
}
