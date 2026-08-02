//! Live: a planned multi-window, multi-pane tree executed by `apply()` over
//! a real `tmux-control::Client` against a real, isolated tmux server, then
//! checked for the captured geometry. `tests/apply.rs` covers the same
//! mechanism on the simplest tree; this is the shape that exercises window
//! placement at non-default indices and the "split the active pane last"
//! reordering, so the assertions here are the geometry ones.

mod support;
use support::{line, IsolatedTmux};

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
    SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};
use phoenix_restore::{apply, plan, RestorePolicy};
use tmux_control::{Client, Transport};

fn pane(index: u32, cwd: &str) -> Pane {
    Pane {
        index: PaneIndex(index),
        cwd: cwd.into(),
        program: CapturedProgram::new("zsh", vec![]),
        content: None,
    }
}

/// `name` with `args` run on `client`, as sorted lines — every assertion
/// here is about the set of windows or panes, never their listing order.
fn query<T: Transport>(client: &mut Client<T>, name: &'static str, args: [&str; 4]) -> Vec<String> {
    let mut lines: Vec<String> = client
        .execute(&line(name, args))
        .unwrap_or_else(|e| panic!("{name} failed: {e}"))
        .lines
        .iter()
        .map(|l| String::from_utf8(l.clone()).expect("tmux returned a non-UTF-8 line"))
        .collect();
    lines.sort();
    lines
}

fn sorted(expected: [&str; 3]) -> Vec<String> {
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    expected
}

#[test]
fn a_planned_multi_window_multi_pane_tree_applies_cleanly_to_a_live_server() {
    let harness = IsolatedTmux::new("restore-plan-apply");
    let mut client = harness.connect();

    // tmux's `-c <cwd>` errors if the directory doesn't actually exist, so
    // these need to be real, distinct directories — which also lets the
    // panes be told apart after restore without relying on numeric pane
    // index (this crate deliberately never targets panes by index — see
    // `panes_active_last`'s doc comment).
    let base =
        std::env::temp_dir().join(format!("phoenix-restore-test-apply-{}", std::process::id()));
    let cwd_active = base.join("pane-active");
    let cwd_b = base.join("pane-b");
    let cwd_c = base.join("pane-c");
    for dir in [&base, &cwd_active, &cwd_b, &cwd_c] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // tmux reports `pane_current_path` fully resolved (macOS's TMPDIR is a
    // `/var` -> `/private/var` symlink) — canonicalize so the expected
    // strings match what's actually reported back.
    let base = base.canonicalize().unwrap();
    let cwd_active = cwd_active.canonicalize().unwrap();
    let cwd_b = cwd_b.canonicalize().unwrap();
    let cwd_c = cwd_c.canonicalize().unwrap();

    let window0 = Window::new(
        WindowIndex(0),
        WindowName::parse("shell").unwrap(),
        Layout::parse("unused,80x24,0,0,0").unwrap(),
        NonEmpty::singleton(pane(0, base.to_str().unwrap())),
        PaneIndex(0),
    )
    .unwrap();

    // The active pane (index 0) is originally *first* in the snapshot, to
    // actually exercise the "reorder so the active pane is split last"
    // logic rather than trivially matching creation order.
    let panes1 = NonEmpty::from_vec(vec![
        pane(0, cwd_active.to_str().unwrap()),
        pane(1, cwd_b.to_str().unwrap()),
        pane(2, cwd_c.to_str().unwrap()),
    ])
    .unwrap();
    let window1 = Window::new(
        WindowIndex(5),
        WindowName::parse("editor").unwrap(),
        // A real layout string captured live from an actual 3-pane tmux
        // window — tmux validates the leading checksum against the rest of
        // the string, so this can't be hand-typed.
        Layout::parse("77dd,100x30,0,0{50x30,0,0,0,49x30,51,0[49x15,51,0,1,49x14,51,16,2]}")
            .unwrap(),
        panes1,
        PaneIndex(0),
    )
    .unwrap();

    let session = Session::new(
        SessionName::parse("restored").unwrap(),
        NonEmpty::new(window0, vec![window1]),
        WindowIndex(5),
    )
    .unwrap();

    let snapshot = Snapshot {
        format_version: FormatVersion::CURRENT,
        tmux_version: TmuxVersion { major: 3, minor: 5 },
        captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
        sessions: NonEmpty::singleton(session),
    };

    let restore_plan = plan(&snapshot, &RestorePolicy::default());
    let outcome = apply(&mut client, &restore_plan).expect("apply failed");
    assert_eq!(
        outcome.executed + outcome.skipped_move_window,
        restore_plan.commands.len()
    );

    let windows = query(
        &mut client,
        "list-windows",
        [
            "-t",
            "restored",
            "-F",
            "#{window_index} #{window_name} #{window_active}",
        ],
    );
    assert_eq!(
        windows,
        vec!["0 shell 0".to_string(), "5 editor 1".to_string()],
        "window indices, names, and the active flag should match the snapshot"
    );

    let panes = query(
        &mut client,
        "list-panes",
        [
            "-t",
            "restored:5",
            "-F",
            "#{pane_current_path} #{pane_active}",
        ],
    );
    assert_eq!(
        panes,
        sorted([
            &format!("{} 1", cwd_active.display()),
            &format!("{} 0", cwd_b.display()),
            &format!("{} 0", cwd_c.display()),
        ]),
        "window 5 should have 3 panes, each in its captured cwd, with the originally-active one active"
    );

    let sizes = query(
        &mut client,
        "list-panes",
        ["-t", "restored:5", "-F", "#{pane_width}x#{pane_height}"],
    );
    assert_eq!(
        sizes,
        sorted(["50x30", "49x15", "49x14"]),
        "select-layout should reproduce the captured geometry"
    );

    client
        .execute(&line("kill-session", ["-t", "restored"]))
        .expect("cleanup kill-session failed");
    client.close();
    let _ = std::fs::remove_dir_all(&base);
}
