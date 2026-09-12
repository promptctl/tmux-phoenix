//! Tests for ticket `.6`: the free-function command surface (subscribe/
//! unsubscribe, pane action, client flags) and version gating.

use tmux_control::commands::{
    clear_flags, query_tmux_version, require_version, set_flags, set_no_output, set_pane_action,
    subscribe, unsubscribe, PaneAction,
};
use tmux_control::{Client, PaneId, TmuxError, TmuxVersion};

const OK_REPLY: &str = "%begin 1 1 1\n%end 1 1 1\n";

#[test]
fn subscribe_sends_name_what_and_format_as_one_argument() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    subscribe(&mut client, "sub1", "%*", "#{pane_dead}").unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec![r##"refresh-client -B "sub1:%*:#{pane_dead}""##.to_string()]
    );
}

#[test]
fn subscribe_encodes_an_apostrophe_in_an_argument() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    subscribe(&mut client, "it's-a-sub", "", "#{session_name}").unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec![r##"refresh-client -B "it's-a-sub::#{session_name}""##.to_string()]
    );
}

#[test]
fn unsubscribe_builds_the_name_only_form() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    unsubscribe(&mut client, "sub1").unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec!["refresh-client -B sub1".to_string()]
    );
}

#[test]
fn set_pane_action_sends_the_whole_pane_colon_action_token_as_one_argument() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    set_pane_action(&mut client, PaneId(5), PaneAction::Pause).unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec![r#"refresh-client -A "%5:pause""#.to_string()]
    );
}

#[test]
fn pane_action_variants_map_to_spec_13_strings() {
    for (action, expected) in [
        (PaneAction::On, "on"),
        (PaneAction::Off, "off"),
        (PaneAction::Pause, "pause"),
        (PaneAction::Continue, "continue"),
    ] {
        let (transport, state) = MockTransport::new(vec![OK_REPLY]);
        let mut client = Client::new(transport);
        set_pane_action(&mut client, PaneId(1), action).unwrap();
        assert_eq!(
            *state.sent.borrow(),
            vec![format!(r#"refresh-client -A "%1:{expected}""#)]
        );
    }
}

#[test]
fn set_flags_joins_flags_with_commas_unquoted() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    set_flags(&mut client, &["no-output", "read-only"]).unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec!["refresh-client -f no-output,read-only".to_string()]
    );
}

#[test]
fn clear_flags_prefixes_each_flag_with_a_bang() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    clear_flags(&mut client, &["no-output"]).unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec!["refresh-client -f !no-output".to_string()]
    );
}

#[test]
fn set_no_output_is_set_flags_with_exactly_that_flag() {
    let (transport, state) = MockTransport::new(vec![OK_REPLY]);
    let mut client = Client::new(transport);
    set_no_output(&mut client).unwrap();
    assert_eq!(
        *state.sent.borrow(),
        vec!["refresh-client -f no-output".to_string()]
    );
}

#[test]
fn query_tmux_version_parses_the_display_message_reply() {
    let (transport, _state) = MockTransport::new(vec!["%begin 1 1 1\n3.5a\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    let version = query_tmux_version(&mut client).unwrap();
    assert_eq!(version, TmuxVersion { major: 3, minor: 5 });
}

#[test]
fn query_tmux_version_fails_loudly_on_unparseable_reply() {
    let (transport, _state) = MockTransport::new(vec!["%begin 1 1 1\nnot a version\n%end 1 1 1\n"]);
    let mut client = Client::new(transport);
    let err = query_tmux_version(&mut client).unwrap_err();
    match err {
        TmuxError::VersionProbeFailed { output } => {
            assert_eq!(output, vec![b"not a version".to_vec()]);
        }
        other => panic!("expected VersionProbeFailed, got {other:?}"),
    }
}

#[test]
fn require_version_allows_equal_and_newer() {
    let need = TmuxVersion { major: 3, minor: 5 };
    assert!(require_version("op", TmuxVersion { major: 3, minor: 5 }, need).is_ok());
    assert!(require_version("op", TmuxVersion { major: 3, minor: 9 }, need).is_ok());
    assert!(require_version("op", TmuxVersion { major: 4, minor: 0 }, need).is_ok());
}

#[test]
fn require_version_rejects_older_with_a_named_error() {
    let need = TmuxVersion { major: 3, minor: 5 };
    let have = TmuxVersion { major: 3, minor: 2 };
    let err = require_version("fancy-op", have, need).unwrap_err();
    match err {
        TmuxError::UnsupportedTmuxVersion {
            operation,
            required,
            have: got,
        } => {
            assert_eq!(operation, "fancy-op");
            assert_eq!(required, need);
            assert_eq!(got, have);
        }
        other => panic!("expected UnsupportedTmuxVersion, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Live tmux integration
// ---------------------------------------------------------------------------

mod support;
use support::{line, IsolatedTmux, MockTransport, NO_ARGS};
use tmux_control::SpawnTransport;

#[test]
fn live_tmux_query_version_returns_a_real_version_at_least_the_crate_floor() {
    let harness = IsolatedTmux::new("query-version");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &tmux_control::SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    let mut client = Client::connect(transport).expect("handshake failed against real tmux");

    let version = query_tmux_version(&mut client).expect("version probe failed");
    assert!(
        version >= tmux_control::MIN_TMUX_VERSION,
        "test machine's tmux ({version:?}) is below this crate's own floor ({:?})",
        tmux_control::MIN_TMUX_VERSION
    );

    client.close();
}

#[test]
fn live_tmux_subscribe_produces_a_subscription_changed_notification() {
    let harness = IsolatedTmux::new("subscribe");
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &tmux_control::SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    let mut client = Client::connect(transport).expect("handshake failed against real tmux");

    subscribe(&mut client, "phoenix-sub", "", "#{session_windows}").expect("subscribe failed");

    // The subscription timer fires at most once per second (SPEC §14) and
    // only reports a *change* — add a window so the window count changes,
    // then poll until %subscription-changed shows up. Deliberately not
    // renaming the session itself: IsolatedTmux's Drop cleans up by the
    // session's original name, and a rename would break that.
    client.execute(&line("new-window", NO_ARGS)).unwrap();

    let mut found = false;
    for _ in 0..30 {
        let _ = client.execute(&line("list-sessions", NO_ARGS));
        if client.drain_notifications().iter().any(|m| {
            matches!(
                m,
                tmux_control::ServerMessage::SubscriptionChanged { name, .. } if name == "phoenix-sub"
            )
        }) {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        found,
        "expected a %subscription-changed notification for phoenix-sub"
    );

    unsubscribe(&mut client, "phoenix-sub").unwrap();
    client.close();
}
