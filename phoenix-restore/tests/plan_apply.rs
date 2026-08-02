//! Live sanity check: this crate promises `--dry-run` prints the exact
//! string that later gets run (DESIGN.md §6), so the strings themselves
//! need to be real, valid tmux syntax — not just internally-consistent
//! fixtures. This runs each rendered [`TmuxCommand`] against a real,
//! freshly-started (never pre-created) tmux server via a plain shell, the
//! same way a human pasting `--dry-run`'s output would, and checks the
//! resulting geometry. Actually wiring this through `tmux-control::Client`
//! is tmux-restore-qll.2's job — this only proves the command strings
//! themselves are correct.

use std::process::Command;

use phoenix_core::{
    CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session,
    SessionName, Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};
use phoenix_restore::{plan, RestorePolicy};

struct FreshTmuxServer {
    socket: String,
}

impl FreshTmuxServer {
    fn new(name: &str) -> Self {
        let socket = format!(
            "/tmp/tmux-phoenix-test-restore-{name}-{}",
            std::process::id()
        );
        let _ = std::fs::remove_file(&socket);
        Self { socket }
    }

    /// Runs `tmux -S <socket> <command>` through `sh -c`, the same way a
    /// human pasting `--dry-run`'s printed line would — `tmux_escape`'s
    /// quoting only means anything once a real shell parses it.
    ///
    /// `move-window` is allowed to fail: `TmuxCommand::MoveWindow`'s doc
    /// comment documents that it can legitimately hit tmux's "same index"
    /// error when the window already landed on the captured index (this
    /// varies by the test machine's own `base-index` config) — swallowing
    /// that here is standing in for the tolerance tmux-restore-qll.2's real
    /// executor is documented to need.
    fn run(&self, command: &str) {
        let full = format!("tmux -S '{}' {command}", self.socket);
        let status = Command::new("sh")
            .args(["-c", &full])
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn sh for {full:?}: {e}"));
        if !status.success() && !command.starts_with("move-window") {
            panic!("command failed: {full}");
        }
    }

    fn output(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-S", &self.socket])
            .args(args)
            .output()
            .expect("failed to run tmux");
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for FreshTmuxServer {
    fn drop(&mut self) {
        // Kill only the one session this test creates, never the whole
        // server: a tmux server with zero sessions exits on its own by
        // default.
        let _ = Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", "restored"])
            .status();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn pane(index: u32, cwd: &str) -> Pane {
    Pane {
        index: PaneIndex(index),
        cwd: cwd.into(),
        program: CapturedProgram::new("zsh", vec![]),
        content: None,
    }
}

#[test]
fn a_planned_multi_window_multi_pane_tree_applies_cleanly_to_a_fresh_server() {
    let server = FreshTmuxServer::new("apply");

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

    let restore_plan = plan(&snapshot, &RestorePolicy);
    for cmd in &restore_plan.commands {
        server.run(&cmd.to_command_string());
    }

    let windows = server.output(&[
        "list-windows",
        "-t",
        "restored",
        "-F",
        "#{window_index} #{window_name} #{window_active}",
    ]);
    let mut window_lines: Vec<&str> = windows.lines().collect();
    window_lines.sort();
    assert_eq!(
        window_lines,
        vec!["0 shell 0", "5 editor 1"],
        "window indices, names, and the active flag should match the snapshot"
    );

    let panes = server.output(&[
        "list-panes",
        "-t",
        "restored:5",
        "-F",
        "#{pane_current_path} #{pane_active}",
    ]);
    let mut pane_lines: Vec<&str> = panes.lines().collect();
    pane_lines.sort();
    let mut expected_panes = vec![
        format!("{} 1", cwd_active.display()),
        format!("{} 0", cwd_b.display()),
        format!("{} 0", cwd_c.display()),
    ];
    expected_panes.sort();
    assert_eq!(
        pane_lines, expected_panes,
        "window 5 should have 3 panes, each in its captured cwd, with the originally-active one active"
    );

    let geometry = server.output(&[
        "list-panes",
        "-t",
        "restored:5",
        "-F",
        "#{pane_width}x#{pane_height}",
    ]);
    let mut sizes: Vec<&str> = geometry.lines().collect();
    sizes.sort();
    let mut expected = vec!["50x30", "49x15", "49x14"];
    expected.sort();
    assert_eq!(
        sizes, expected,
        "select-layout should reproduce the captured geometry"
    );

    let _ = std::fs::remove_dir_all(&base);
}
