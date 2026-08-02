//! Live: `apply()` driven through a real `tmux-control::Client` against a
//! real, isolated tmux server — the actual mechanism (not the raw-shell
//! stand-in `tests/plan_apply.rs` uses to validate the command strings
//! themselves).

mod support;
use support::{line, IsolatedTmux};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneContent, PaneId,
    PaneIndex, Session, SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};
use phoenix_restore::{apply, plan, RestorePolicy};

fn pane(index: u32, cwd: &str) -> Pane {
    Pane {
        index: PaneIndex(index),
        cwd: cwd.into(),
        program: CapturedProgram::new("zsh", vec![]),
        content: None,
    }
}

#[test]
fn apply_rebuilds_the_snapshot_into_a_new_session_on_a_live_connection() {
    let harness = IsolatedTmux::new("restore-apply");
    let mut client = harness.connect();

    let base =
        std::env::temp_dir().join(format!("phoenix-restore-apply-live-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    // tmux reports pane_current_path fully resolved (macOS's TMPDIR is a
    // /var -> /private/var symlink).
    let base = base.canonicalize().unwrap();

    let panes = NonEmpty::from_vec(vec![
        pane(0, base.to_str().unwrap()),
        pane(1, base.to_str().unwrap()),
    ])
    .unwrap();
    let window = Window::new(
        WindowIndex(0),
        WindowName::parse("shell").unwrap(),
        // A real 2-pane layout captured live — tmux validates the leading
        // checksum against the rest of the string, so this can't be
        // hand-typed (this window has 2 panes, so the plan does emit a
        // select-layout for it).
        Layout::parse("c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap(),
        panes,
        PaneIndex(1),
    )
    .unwrap();
    let session = Session::new(
        SessionName::parse("restored-live").unwrap(),
        NonEmpty::singleton(window),
        WindowIndex(0),
    )
    .unwrap();
    let snapshot = Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version: TmuxVersion { major: 3, minor: 5 },
        captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
        sessions: NonEmpty::singleton(session),
    };

    let restore_plan = plan(&snapshot, &RestorePolicy);
    let outcome = apply(&mut client, &restore_plan).expect("apply failed");
    assert_eq!(
        outcome.executed + outcome.skipped_move_window,
        restore_plan.commands.len()
    );

    let panes_out = client
        .execute(&line(
            "list-panes",
            ["-t", "restored-live:0", "-F", "#{pane_index}"],
        ))
        .expect("list-panes failed");
    assert_eq!(
        panes_out.lines.len(),
        2,
        "restored session should have 2 panes"
    );

    client
        .execute(&line("kill-session", ["-t", "restored-live"]))
        .expect("cleanup kill-session failed");
    client.close();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn apply_replays_each_panes_captured_content_distinctly() {
    let harness = IsolatedTmux::new("restore-apply-content");
    let mut client = connect(&harness);

    let base = std::env::temp_dir().join(format!(
        "phoenix-restore-apply-content-live-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();

    let mut pane_a = pane(0, base.to_str().unwrap());
    pane_a.content = Some(PaneContent::new(
        PaneId(1),
        1,
        1,
        vec!["DISTINCTIVE-CONTENT-PANE-A".to_string()],
        vec![],
    ));
    let mut pane_b = pane(1, base.to_str().unwrap());
    pane_b.content = Some(PaneContent::new(
        PaneId(2),
        2,
        2,
        vec!["DISTINCTIVE-CONTENT-PANE-B".to_string()],
        vec![],
    ));

    let panes = NonEmpty::new(pane_a, vec![pane_b]);
    let window = Window::new(
        WindowIndex(0),
        WindowName::parse("shell").unwrap(),
        Layout::parse("c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap(),
        panes,
        PaneIndex(1),
    )
    .unwrap();
    let session = Session::new(
        SessionName::parse("restored-content").unwrap(),
        NonEmpty::singleton(window),
        WindowIndex(0),
    )
    .unwrap();
    let snapshot = Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version: TmuxVersion { major: 3, minor: 5 },
        captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
        sessions: NonEmpty::singleton(session),
    };

    let restore_plan = plan(&snapshot, &RestorePolicy);
    apply(&mut client, &restore_plan).expect("apply failed");

    // Give the shells a moment to actually run their `cat` before capturing.
    let mut found_a = false;
    let mut found_b = false;
    for _ in 0..30 {
        let panes_out = client
            .execute("list-panes -t restored-content:0 -F '#{pane_index}'")
            .unwrap();
        let indices: Vec<String> = panes_out
            .lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();

        let mut all_text = String::new();
        for idx in &indices {
            let out = client
                .execute(&format!("capture-pane -p -t 'restored-content:0.{idx}'"))
                .unwrap();
            for line in &out.lines {
                all_text.push_str(&String::from_utf8_lossy(line));
                all_text.push('\n');
            }
        }
        found_a = all_text.contains("DISTINCTIVE-CONTENT-PANE-A");
        found_b = all_text.contains("DISTINCTIVE-CONTENT-PANE-B");
        if found_a && found_b {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(found_a, "pane a's replayed content should appear somewhere");
    assert!(found_b, "pane b's replayed content should appear somewhere");

    client
        .execute("kill-session -t restored-content")
        .expect("cleanup kill-session failed");
    client.close();
    let _ = std::fs::remove_dir_all(&base);
}
