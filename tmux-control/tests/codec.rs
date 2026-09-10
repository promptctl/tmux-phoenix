//! Conformance tests for the pure codec: SPEC's own documented examples,
//! the guard-block state machine's edge cases, and the full ServerMessage
//! union. Malformed-input expectations intentionally diverge from the
//! `promptctl/tmux-control-mode-js` reference test suite where DESIGN.md
//! calls for a stronger guarantee (`Unknown`/`ProtocolError` instead of a
//! silent drop) — see `message.rs` module docs for why.

use tmux_control::{Codec, Guard, Layout, PaneId, ServerMessage, SessionId, WindowId};

fn feed(input: &str) -> Vec<ServerMessage> {
    let mut codec = Codec::new();
    codec.feed(input.as_bytes())
}

// ---------------------------------------------------------------------------
// SPEC §5.2's own documented example
// ---------------------------------------------------------------------------

#[test]
fn spec_5_2_example_block() {
    let input = "%begin 1363006971 2 1\n\
                  0: ksh* (1 panes) [80x24] [layout b25f,80x24,0,0,2] @2 (active)\n\
                  %end 1363006971 2 1\n";
    let messages = feed(input);
    assert_eq!(
        messages,
        vec![
            ServerMessage::GuardBegin(Guard {
                timestamp: 1363006971,
                command_number: 2,
                flags: 1
            }),
            ServerMessage::CommandOutput {
                command_number: 2,
                line: b"0: ksh* (1 panes) [80x24] [layout b25f,80x24,0,0,2] @2 (active)".to_vec(),
            },
            ServerMessage::GuardEnd(Guard {
                timestamp: 1363006971,
                command_number: 2,
                flags: 1
            }),
        ]
    );
}

#[test]
fn spec_5_3_parse_error_example() {
    let input = "%begin 1363006971 3 1\nparse error: unknown command\n%error 1363006971 3 1\n";
    let messages = feed(input);
    assert_eq!(
        messages,
        vec![
            ServerMessage::GuardBegin(Guard {
                timestamp: 1363006971,
                command_number: 3,
                flags: 1
            }),
            ServerMessage::CommandOutput {
                command_number: 3,
                line: b"parse error: unknown command".to_vec(),
            },
            ServerMessage::GuardError(Guard {
                timestamp: 1363006971,
                command_number: 3,
                flags: 1
            }),
        ]
    );
}

// ---------------------------------------------------------------------------
// decode_octal (SPEC §10), exercised through %output
// ---------------------------------------------------------------------------

#[test]
fn output_decodes_newline_escape() {
    let messages = feed("%output %1 hello\\012\n");
    assert_eq!(
        messages,
        vec![ServerMessage::Output {
            pane: PaneId(1),
            data: b"hello\n".to_vec()
        }]
    );
}

#[test]
fn output_decodes_ansi_escape() {
    let messages = feed("%output %1 \\033[1;32mgreen\n");
    match &messages[0] {
        ServerMessage::Output { data, .. } => assert_eq!(data[0], 27),
        other => panic!("expected Output, got {other:?}"),
    }
}

#[test]
fn output_decodes_backslash_escape() {
    let messages = feed("%output %1 \\134backslash\n");
    match &messages[0] {
        ServerMessage::Output { data, .. } => assert_eq!(data[0], 0x5c),
        other => panic!("expected Output, got {other:?}"),
    }
}

#[test]
fn output_decodes_null_byte() {
    let messages = feed("%output %1 \\000rest\n");
    match &messages[0] {
        ServerMessage::Output { data, .. } => assert_eq!(data[0], 0),
        other => panic!("expected Output, got {other:?}"),
    }
}

#[test]
fn output_decodes_high_byte() {
    let messages = feed("%output %2 \\377\n");
    match &messages[0] {
        ServerMessage::Output { pane, data } => {
            assert_eq!(*pane, PaneId(2));
            assert_eq!(data[0], 0xff);
        }
        other => panic!("expected Output, got {other:?}"),
    }
}

#[test]
fn malformed_escape_decodes_to_question_mark() {
    // `\` followed by a non-octal digit ('9') is a malformed escape.
    let messages = feed("%output %1 \\9tail\n");
    match &messages[0] {
        ServerMessage::Output { data, .. } => {
            // '?' for the failed escape, then '9' and "tail" reprocessed as
            // ordinary bytes (the reference decoder's recovery).
            assert_eq!(data, b"?9tail");
        }
        other => panic!("expected Output, got {other:?}"),
    }
}

#[test]
fn literal_control_byte_in_output_is_dropped() {
    // A literal (unescaped) control byte is transport noise per SPEC §10 —
    // tmux always escapes real control bytes.
    let mut codec = Codec::new();
    let messages = codec.feed(b"%output %1 a\x01b\n");
    match &messages[0] {
        ServerMessage::Output { data, .. } => assert_eq!(data, b"ab"),
        other => panic!("expected Output, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Extended output (SPEC §7.1)
// ---------------------------------------------------------------------------

#[test]
fn extended_output_parses_age_and_data() {
    let messages = feed("%extended-output %1 1000 : hello\\012\n");
    assert_eq!(
        messages,
        vec![ServerMessage::ExtendedOutput {
            pane: PaneId(1),
            age_ms: 1000,
            data: b"hello\n".to_vec()
        }]
    );
}

// ---------------------------------------------------------------------------
// One representative line per remaining SPEC §23 message type
// ---------------------------------------------------------------------------

#[test]
fn pause_and_continue() {
    assert_eq!(
        feed("%pause %3\n"),
        vec![ServerMessage::Pause { pane: PaneId(3) }]
    );
    assert_eq!(
        feed("%continue %3\n"),
        vec![ServerMessage::Continue { pane: PaneId(3) }]
    );
}

#[test]
fn pane_mode_changed() {
    assert_eq!(
        feed("%pane-mode-changed %2\n"),
        vec![ServerMessage::PaneModeChanged { pane: PaneId(2) }]
    );
}

#[test]
fn window_add_and_close() {
    assert_eq!(
        feed("%window-add @1\n"),
        vec![ServerMessage::WindowAdd {
            window: WindowId(1)
        }]
    );
    assert_eq!(
        feed("%window-close @1\n"),
        vec![ServerMessage::WindowClose {
            window: WindowId(1)
        }]
    );
}

#[test]
fn window_renamed_carries_name() {
    assert_eq!(
        feed("%window-renamed @1 bash\n"),
        vec![ServerMessage::WindowRenamed {
            window: WindowId(1),
            name: "bash".into()
        }]
    );
}

#[test]
fn window_pane_changed() {
    assert_eq!(
        feed("%window-pane-changed @1 %1\n"),
        vec![ServerMessage::WindowPaneChanged {
            window: WindowId(1),
            pane: PaneId(1)
        }]
    );
}

#[test]
fn unlinked_window_events() {
    assert_eq!(
        feed("%unlinked-window-add @5\n"),
        vec![ServerMessage::UnlinkedWindowAdd {
            window: WindowId(5)
        }]
    );
    assert_eq!(
        feed("%unlinked-window-close @5\n"),
        vec![ServerMessage::UnlinkedWindowClose {
            window: WindowId(5)
        }]
    );
    assert_eq!(
        feed("%unlinked-window-renamed @5 scratch\n"),
        vec![ServerMessage::UnlinkedWindowRenamed {
            window: WindowId(5),
            name: "scratch".into()
        }]
    );
}

#[test]
fn layout_change() {
    assert_eq!(
        feed("%layout-change @0 4b5a,220x50,0,0,%1 4b5a,220x50,0,0,%1 *\n"),
        vec![ServerMessage::LayoutChange {
            window: WindowId(0),
            layout: Layout("4b5a,220x50,0,0,%1".into()),
            visible: Layout("4b5a,220x50,0,0,%1".into()),
            flags: "*".into(),
        }]
    );
}

#[test]
fn session_changed_and_renamed() {
    assert_eq!(
        feed("%session-changed $1 main\n"),
        vec![ServerMessage::SessionChanged {
            session: SessionId(1),
            name: "main".into()
        }]
    );
    // SPEC §25: code sends id+name despite the man page documenting name-only.
    assert_eq!(
        feed("%session-renamed $1 work\n"),
        vec![ServerMessage::SessionRenamed {
            session: SessionId(1),
            name: "work".into()
        }]
    );
}

#[test]
fn sessions_changed_takes_no_args() {
    assert_eq!(
        feed("%sessions-changed\n"),
        vec![ServerMessage::SessionsChanged]
    );
}

#[test]
fn session_window_changed() {
    assert_eq!(
        feed("%session-window-changed $1 @2\n"),
        vec![ServerMessage::SessionWindowChanged {
            session: SessionId(1),
            window: WindowId(2)
        }]
    );
}

#[test]
fn client_session_changed_and_detached() {
    assert_eq!(
        feed("%client-session-changed /dev/pts/0 $1 main\n"),
        vec![ServerMessage::ClientSessionChanged {
            client: "/dev/pts/0".into(),
            session: SessionId(1),
            name: "main".into(),
        }]
    );
    assert_eq!(
        feed("%client-detached /dev/pts/1\n"),
        vec![ServerMessage::ClientDetached {
            client: "/dev/pts/1".into()
        }]
    );
}

#[test]
fn paste_buffer_events() {
    assert_eq!(
        feed("%paste-buffer-changed buffer0\n"),
        vec![ServerMessage::PasteBufferChanged {
            name: "buffer0".into()
        }]
    );
    assert_eq!(
        feed("%paste-buffer-deleted buffer0\n"),
        vec![ServerMessage::PasteBufferDeleted {
            name: "buffer0".into()
        }]
    );
}

#[test]
fn message_and_config_error() {
    assert_eq!(
        feed("%message Window @1 created\n"),
        vec![ServerMessage::Message {
            text: "Window @1 created".into()
        }]
    );
    assert_eq!(
        feed("%config-error Unknown option: foo\n"),
        vec![ServerMessage::ConfigError {
            text: "Unknown option: foo".into()
        }]
    );
}

#[test]
fn exit_with_and_without_reason() {
    assert_eq!(feed("%exit\n"), vec![ServerMessage::Exit { reason: None }]);
    assert_eq!(
        feed("%exit detached\n"),
        vec![ServerMessage::Exit {
            reason: Some("detached".into())
        }]
    );
}

// ---------------------------------------------------------------------------
// Subscription-changed (SPEC §7.9): the four session/window/pane arities
// ---------------------------------------------------------------------------

#[test]
fn subscription_changed_pane_level() {
    assert_eq!(
        feed("%subscription-changed my-sub $1 @1 0 %1 : pane-value\n"),
        vec![ServerMessage::SubscriptionChanged {
            name: "my-sub".into(),
            session: Some(SessionId(1)),
            window: Some(WindowId(1)),
            window_index: Some(0),
            pane: Some(PaneId(1)),
            value: "pane-value".into(),
        }]
    );
}

#[test]
fn subscription_changed_window_level_pane_is_dash() {
    assert_eq!(
        feed("%subscription-changed window-sub $1 @1 0 - : window-title\n"),
        vec![ServerMessage::SubscriptionChanged {
            name: "window-sub".into(),
            session: Some(SessionId(1)),
            window: Some(WindowId(1)),
            window_index: Some(0),
            pane: None,
            value: "window-title".into(),
        }]
    );
}

#[test]
fn subscription_changed_session_level() {
    assert_eq!(
        feed("%subscription-changed session-sub $1 - - - : session-name\n"),
        vec![ServerMessage::SubscriptionChanged {
            name: "session-sub".into(),
            session: Some(SessionId(1)),
            window: None,
            window_index: None,
            pane: None,
            value: "session-name".into(),
        }]
    );
}

#[test]
fn subscription_changed_global_all_dashes() {
    assert_eq!(
        feed("%subscription-changed global-sub - - - - : global-value\n"),
        vec![ServerMessage::SubscriptionChanged {
            name: "global-sub".into(),
            session: None,
            window: None,
            window_index: None,
            pane: None,
            value: "global-value".into(),
        }]
    );
}

#[test]
fn subscription_changed_value_may_contain_spaces() {
    assert_eq!(
        feed("%subscription-changed multi-sub $2 @3 1 %5 : complex value with spaces\n"),
        vec![ServerMessage::SubscriptionChanged {
            name: "multi-sub".into(),
            session: Some(SessionId(2)),
            window: Some(WindowId(3)),
            window_index: Some(1),
            pane: Some(PaneId(5)),
            value: "complex value with spaces".into(),
        }]
    );
}

// ---------------------------------------------------------------------------
// Block purity (SPEC §6's central invariant)
// ---------------------------------------------------------------------------

#[test]
fn percent_prefixed_lines_inside_block_are_output_not_notifications() {
    let input = "%begin 1699900000 8 0\n%sessions-changed\n%end 1699900000 8 0\n";
    let messages = feed(input);
    assert!(!messages
        .iter()
        .any(|m| matches!(m, ServerMessage::SessionsChanged)));
    assert!(messages.iter().any(|m| matches!(
        m,
        ServerMessage::CommandOutput { line, .. } if line == b"%sessions-changed"
    )));
}

#[test]
fn bare_pane_id_lines_from_format_output_survive_as_command_output() {
    // list-panes -F '#{pane_id}' emits lines that look exactly like pane ids.
    let input = "%begin 1699900000 9 0\n%5\n%7\n%11\n%end 1699900000 9 0\n";
    let messages = feed(input);
    let output_lines: Vec<&[u8]> = messages
        .iter()
        .filter_map(|m| match m {
            ServerMessage::CommandOutput { line, .. } => Some(line.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(
        output_lines,
        vec![b"%5".as_slice(), b"%7".as_slice(), b"%11".as_slice()]
    );
}

#[test]
fn only_end_and_error_close_the_block() {
    let input = "%begin 1699900000 1 0\n\
                  %output %2 hello\n\
                  %end 1699900000 1 0\n\
                  %output %2 world\n";
    let messages = feed(input);
    assert_eq!(
        messages,
        vec![
            ServerMessage::GuardBegin(Guard {
                timestamp: 1699900000,
                command_number: 1,
                flags: 0
            }),
            ServerMessage::CommandOutput {
                command_number: 1,
                line: b"%output %2 hello".to_vec()
            },
            ServerMessage::GuardEnd(Guard {
                timestamp: 1699900000,
                command_number: 1,
                flags: 0
            }),
            ServerMessage::Output {
                pane: PaneId(2),
                data: b"world".to_vec()
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// Stability: malformed input never wedges or panics
// ---------------------------------------------------------------------------

#[test]
fn malformed_block_terminator_force_closes_instead_of_wedging() {
    let input = "%begin 1699900000 7 0\n%end 1699900000 7\n%window-add @9\n";
    let messages = feed(input);
    assert_eq!(
        messages,
        vec![
            ServerMessage::GuardBegin(Guard {
                timestamp: 1699900000,
                command_number: 7,
                flags: 0
            }),
            ServerMessage::ProtocolError {
                command_number: 7,
                line: b"%end 1699900000 7".to_vec()
            },
            ServerMessage::WindowAdd {
                window: WindowId(9)
            },
        ]
    );
}

#[test]
fn malformed_error_terminator_force_closes_the_same_way() {
    let input = "%begin 1699900000 3 0\n%error 1699900000\n%sessions-changed\n";
    let messages = feed(input);
    assert_eq!(
        messages,
        vec![
            ServerMessage::GuardBegin(Guard {
                timestamp: 1699900000,
                command_number: 3,
                flags: 0
            }),
            ServerMessage::ProtocolError {
                command_number: 3,
                line: b"%error 1699900000".to_vec()
            },
            ServerMessage::SessionsChanged,
        ]
    );
}

#[test]
fn unrecognized_notification_type_degrades_to_unknown() {
    // Deliberate divergence from the JS reference (which silently drops):
    // DESIGN.md's no-silent-failure law wants this observable.
    let messages = feed("%unknown-future-type foo bar baz\n");
    assert_eq!(
        messages,
        vec![ServerMessage::Unknown(
            "%unknown-future-type foo bar baz".into()
        )]
    );
}

#[test]
fn malformed_known_type_outside_block_degrades_to_unknown() {
    let messages = feed("%begin 1699900000 0\n"); // only 2 of 3 required guard fields
    assert_eq!(
        messages,
        vec![ServerMessage::Unknown("%begin 1699900000 0".into())]
    );
}

#[test]
fn non_percent_line_outside_block_is_unknown() {
    let messages = feed("this is not a notification\n");
    assert_eq!(
        messages,
        vec![ServerMessage::Unknown("this is not a notification".into())]
    );
}

#[test]
fn empty_line_outside_block_is_unknown() {
    assert_eq!(feed("\n"), vec![ServerMessage::Unknown(String::new())]);
}

#[test]
fn signed_numeric_fields_do_not_parse() {
    // `str::parse` accepts a leading sign; tmux never emits one.
    assert_eq!(
        feed("%begin +1699900000 0 0\n"),
        vec![ServerMessage::Unknown("%begin +1699900000 0 0".into())]
    );
    assert_eq!(SessionId::parse(b"$+5"), None);
    assert_eq!(PaneId::parse(b"%-3"), None);
}

#[test]
fn very_long_line_does_not_panic() {
    let long_data = "a".repeat(50_000);
    let messages = feed(&format!("%output %1 {long_data}\n"));
    assert_eq!(messages.len(), 1);
    assert!(matches!(messages[0], ServerMessage::Output { .. }));
}

#[test]
fn very_long_unknown_line_does_not_panic() {
    let long_tail = "x".repeat(50_000);
    let messages = feed(&format!("%unknown-type {long_tail}\n"));
    assert_eq!(messages.len(), 1);
    assert!(matches!(messages[0], ServerMessage::Unknown(_)));
}

#[test]
fn empty_feed_produces_nothing() {
    assert_eq!(feed(""), vec![]);
}

// ---------------------------------------------------------------------------
// Buffering behavior
// ---------------------------------------------------------------------------

#[test]
fn chunked_feeding_one_byte_at_a_time_matches_one_shot() {
    let input = "%begin 1699900000 0 0\n%end 1699900000 0 0\n%session-changed $1 main\n";
    let one_shot = feed(input);

    let mut codec = Codec::new();
    let mut chunked = Vec::new();
    for byte in input.as_bytes() {
        chunked.extend(codec.feed(&[*byte]));
    }
    assert_eq!(chunked, one_shot);
}

#[test]
fn crlf_is_tolerated_like_lf() {
    let mut codec = Codec::new();
    let messages = codec.feed(b"%sessions-changed\r\n");
    assert_eq!(messages, vec![ServerMessage::SessionsChanged]);
}

#[test]
fn crlf_mixed_with_lf_across_many_lines_in_one_chunk() {
    // Regression for feed()'s cursor-based rewrite: each line's CRLF check
    // must only look at the byte immediately before that line's own
    // newline, never at a byte left over from a prior line in the same
    // chunk (which would misfire once the buffer stopped being re-sliced to
    // start at 0 for every line).
    let mut codec = Codec::new();
    let messages = codec.feed(b"%sessions-changed\r\n%window-add @1\n%sessions-changed\r\n");
    assert_eq!(
        messages,
        vec![
            ServerMessage::SessionsChanged,
            ServerMessage::WindowAdd {
                window: WindowId(1)
            },
            ServerMessage::SessionsChanged,
        ]
    );
}

#[test]
fn reset_clears_partial_buffer_and_open_block() {
    let mut codec = Codec::new();
    // Partial line, nothing emitted yet.
    assert_eq!(codec.feed(b"%begin 1699900000 0 0"), vec![]);
    codec.reset();
    // A fragment of the reset-away partial line, followed by an unrelated
    // valid line, must not resurrect the old buffer content.
    let messages = codec.feed(b"%sessions-changed\n");
    assert_eq!(messages, vec![ServerMessage::SessionsChanged]);
}

#[test]
fn reset_inside_open_block_clears_active_command() {
    let mut codec = Codec::new();
    let begin = codec.feed(b"%begin 1699900000 42 0\n");
    assert_eq!(
        begin,
        vec![ServerMessage::GuardBegin(Guard {
            timestamp: 1699900000,
            command_number: 42,
            flags: 0
        })]
    );
    codec.reset();
    // No block is open anymore, so a plain line is `Unknown`, not output for
    // command 42.
    let messages = codec.feed(b"this line is outside\n");
    assert_eq!(
        messages,
        vec![ServerMessage::Unknown("this line is outside".into())]
    );
}

// ---------------------------------------------------------------------------
// SPEC §3: id prefix parsing
// ---------------------------------------------------------------------------

#[test]
fn id_prefixes_are_parsed_once_at_the_boundary() {
    assert_eq!(SessionId::parse(b"$42"), Some(SessionId(42)));
    assert_eq!(WindowId::parse(b"@7"), Some(WindowId(7)));
    assert_eq!(PaneId::parse(b"%3"), Some(PaneId(3)));
    // Wrong prefix, no prefix, or non-numeric suffix all fail to parse.
    assert_eq!(SessionId::parse(b"@42"), None);
    assert_eq!(WindowId::parse(b"7"), None);
    assert_eq!(PaneId::parse(b"%abc"), None);
}
