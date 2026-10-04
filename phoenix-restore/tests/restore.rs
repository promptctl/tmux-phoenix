//! Live: capture, `plan` and `apply` over one real connection to a real,
//! isolated tmux server — the acceptance cases of `tmux-laws-a4x.kdi`.

mod support;
use support::{eventually, pane, panes, session, snapshot, window, Server, GEN};

use phoenix_core::{Content, Foreground, HistoryIndicator, NonEmpty, WindowIndex};
use phoenix_restore::{Note, Step};

const WINDOWS: [&str; 4] = [
    "list-windows",
    "-a",
    "-F",
    "#{session_name} #{window_index} #{window_name}",
];

#[test]
fn onto_no_server_the_snapshot_is_all_there_is_and_a_second_restore_plans_nothing() {
    let server = Server::new("empty");
    let saved = snapshot(
        vec![window(1, "editor"), window(2, "logs"), window(3, "shell")],
        vec![
            session("main", &[(1, 2), (0, 1)]),
            session("work", &[(4, 3)]),
        ],
    );

    let first = server.restore(&saved);

    assert_eq!(
        server.session_names(),
        ["main", "work"],
        "the session made to attach with is gone"
    );
    let mut windows = server.lines(&WINDOWS);
    windows.sort();
    assert_eq!(windows, ["main 0 editor", "main 1 logs", "work 4 shell"]);
    assert_eq!(
        server.lines(&[
            "list-windows",
            "-t",
            "=main:",
            "-F",
            "#{window_index} #{window_active}"
        ]),
        ["0 0", "1 1"],
        "a session the plan made shows its saved active window"
    );
    assert_eq!(
        server
            .tmux(&["show-options", "-s", "-v", "@phoenix-generation"])
            .trim(),
        "7"
    );
    assert_eq!(
        server
            .tmux(&["show-options", "-t", "=work:", "-v", "@phoenix-restored"])
            .trim(),
        "7"
    );
    assert!(matches!(first.steps().last(), Some(Step::SetOption { .. })));

    let second = server.restore(&saved);
    assert_eq!(second.steps(), [], "everything is already there");
    assert_eq!(server.lines(&WINDOWS).len(), 3);
}

/// Where a server's own config puts a session's first window (`base-index`).
fn first_index(server: &Server, session: &str) -> u32 {
    let target = format!("={session}:");
    server.lines(&["list-windows", "-t", &target, "-F", "#{window_index}"])[0]
        .parse()
        .unwrap()
}

#[test]
fn a_server_holding_a_saved_session_gets_its_missing_windows_and_the_report_says_what_stayed() {
    let server = Server::holding("partial", "alpha");
    let at = first_index(&server, "alpha");
    let saved = snapshot(
        vec![window(1, "a0"), window(2, "a1"), window(3, "b0")],
        vec![
            session("alpha", &[(at, 1), (at + 1, 2)]),
            session("beta", &[(0, 3)]),
        ],
    );
    let alpha_windows = [
        "list-windows",
        "-t",
        "=alpha:",
        "-F",
        "#{window_index} #{window_id} #{window_name}",
    ];
    // Index and id only: tmux renames a window after what runs in it.
    let placed = |row: &str| row.rsplitn(2, ' ').last().unwrap().to_string();
    let before = server.lines(&alpha_windows);

    let plan = server.restore(&saved);

    assert_eq!(server.session_names(), ["alpha", "beta"]);
    let alpha = server.lines(&alpha_windows);
    assert_eq!(alpha.len(), 3, "{alpha:?}");
    assert_eq!(
        placed(&alpha[0]),
        placed(&before[0]),
        "the window the session already had stays where it was"
    );
    assert!(
        alpha[1].starts_with(&format!("{} ", at + 1)) && alpha[1].ends_with(" a1"),
        "{alpha:?}"
    );
    assert!(
        alpha[2].starts_with(&format!("{} ", at + 2)) && alpha[2].ends_with(" a0"),
        "the window whose saved index was taken lands at the next free one: {alpha:?}"
    );
    assert_eq!(
        server.lines(&[
            "list-windows",
            "-t",
            "=beta:",
            "-F",
            "#{window_index} #{window_name}"
        ]),
        ["0 b0"]
    );

    let alpha = || tmux_control::SessionName::parse("alpha").unwrap();
    for note in [
        Note::SessionPresent { session: alpha() },
        Note::NotFromSnapshot {
            session: alpha(),
            index: WindowIndex(at),
        },
        Note::Relocated {
            session: alpha(),
            saved: WindowIndex(at),
            landed: WindowIndex(at + 2),
        },
    ] {
        assert!(
            plan.notes().contains(&note),
            "missing {note:?} in {:?}",
            plan.notes()
        );
    }
}

#[test]
fn five_windows_join_a_terminals_fresh_session_and_leave_its_window_and_client_alone() {
    let inner = Server::holding("login", "0");
    let host = Server::new("login-host");
    // A real terminal client: `tmux attach` in a pane of a second server, so
    // it has a pty. The target is quoted because the pane's command runs
    // through a shell, where zsh reads a bare `=0` as a command lookup.
    host.tmux(&[
        "new-session",
        "-d",
        "-s",
        "terminal",
        &format!("tmux -S {} attach -t '=0'", inner.socket),
    ]);
    let terminal = [
        "list-clients",
        "-F",
        "#{client_control_mode} #{session_name} #{window_id}",
    ];
    let attached = eventually(|| inner.lines(&terminal), |clients| clients.len() == 1);
    assert_eq!(attached.len(), 1, "the terminal never attached");

    let at = first_index(&inner, "0");
    let windows = (0..5)
        .map(|n| window(10 + n, &format!("saved{n}")))
        .collect();
    let links: Vec<(u32, u32)> = (0..5).map(|n| (at + n, 10 + n)).collect();
    inner.restore(&snapshot(windows, vec![session("0", &links)]));

    let listed = inner.lines(&[
        "list-windows",
        "-t",
        "=0:",
        "-F",
        "#{window_index} #{window_name}",
    ]);
    let expected: Vec<String> = (1..5)
        .map(|n| format!("{} saved{n}", at + n))
        .chain([format!("{} saved0", at + 5)])
        .collect();
    assert_eq!(listed.len(), 6, "{listed:?}");
    assert_eq!(
        listed[1..],
        expected,
        "each saved window at its index, the one whose index was taken at the next free one"
    );
    assert_eq!(inner.session_names(), ["0"]);
    assert_eq!(
        eventually(|| inner.lines(&terminal), |clients| clients.len() == 1),
        attached,
        "the terminal is still attached, still showing the window it showed"
    );
    assert_eq!(
        host.tmux(&["list-panes", "-t", "=terminal:", "-F", "#{pane_dead}"])
            .trim(),
        "0",
        "the terminal's tmux client is still running"
    );
}

#[test]
fn a_window_two_sessions_share_is_built_once_and_linked_into_both() {
    let server = Server::new("grouped");
    let saved = snapshot(
        vec![window(1, "shared")],
        vec![session("left", &[(0, 1)]), session("right", &[(3, 1)])],
    );

    server.restore(&saved);

    let mut links = server.lines(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name} #{window_index} #{window_id}",
    ]);
    links.sort();
    assert_eq!(
        links.len(),
        2,
        "one link each, and nothing left of what `right` was made with: {links:?}"
    );
    let id = |row: &str| row.rsplit(' ').next().unwrap().to_string();
    assert_eq!(id(&links[0]), id(&links[1]), "one window: {links:?}");
    assert!(
        links[0].starts_with("left 0 ") && links[1].starts_with("right 3 "),
        "{links:?}"
    );
    assert_eq!(
        server
            .tmux(&[
                "show-options",
                "-w",
                "-t",
                &id(&links[0]),
                "-v",
                "@phoenix-window"
            ])
            .trim(),
        "7:@1"
    );
}

#[test]
fn a_three_pane_window_comes_back_in_pane_order_with_its_active_pane_active() {
    let server = Server::holding("panes", "mine");
    let base = std::env::temp_dir().join(format!("phoenix-restore-panes-{}", std::process::id()));
    let dirs: Vec<String> = ["first", "second", "third"]
        .iter()
        .map(|name| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            // tmux reports `pane_current_path` resolved (macOS's TMPDIR is
            // behind a symlink).
            dir.canonicalize().unwrap().to_str().unwrap().to_string()
        })
        .collect();
    // The first pane is the active one, so pane order and "which pane is
    // active" cannot both come out right by creation order alone.
    let saved = snapshot(
        vec![panes(
            1,
            "editor",
            "77dd,100x30,0,0{50x30,0,0,0,49x30,51,0[49x15,51,0,1,49x14,51,16,2]}",
            vec![pane(0, &dirs[0]), pane(1, &dirs[1]), pane(2, &dirs[2])],
            0,
        )],
        vec![session("restored", &[(5, 1)])],
    );

    server.restore(&saved);

    let listing = [
        "list-panes",
        "-t",
        "=restored:=5",
        "-F",
        "#{pane_current_path} #{pane_active} #{pane_width}x#{pane_height}",
    ];
    let listed = eventually(
        || server.lines(&listing),
        |rows| rows.iter().all(|row| !row.starts_with(' ')),
    );
    assert_eq!(
        listed,
        [
            format!("{} 1 50x30", dirs[0]),
            format!("{} 0 49x15", dirs[1]),
            format!("{} 0 49x14", dirs[2]),
        ],
        "each pane in its saved place, cwd and size, the saved active one active"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn content_and_program_come_back_in_the_pane_they_were_saved_from() {
    let server = Server::holding("content", "mine");
    let content = |line: &str| Content::Captured {
        indicator: HistoryIndicator {
            history_size: 1,
            history_bytes: 1,
        },
        scrollback: vec![line.to_string()],
        visible: vec![],
    };
    let mut left = pane(0, "/tmp");
    left.content = content("SAVED-CONTENT-LEFT");
    left.foreground = Foreground::Program {
        argv: NonEmpty::new("echo".to_string(), vec!["RELAUNCHED LEFT".to_string()]),
    };
    let mut right = pane(1, "/tmp");
    right.content = content("SAVED-CONTENT-RIGHT");
    let saved = snapshot(
        vec![panes(
            1,
            "shell",
            "c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]",
            vec![left, right],
            1,
        )],
        vec![session("restored", &[(0, 1)])],
    );

    server.restore(&saved);

    let ids = server.lines(&["list-panes", "-t", "=restored:=0", "-F", "#{pane_id}"]);
    let text = |pane: &str| server.tmux(&["capture-pane", "-p", "-t", pane]);
    let left = eventually(
        || text(&ids[0]),
        |text| text.contains("SAVED-CONTENT-LEFT") && text.lines().any(|l| l == "RELAUNCHED LEFT"),
    );
    let right = eventually(
        || text(&ids[1]),
        |text| text.contains("SAVED-CONTENT-RIGHT"),
    );
    assert!(
        left.contains("SAVED-CONTENT-LEFT") && left.lines().any(|l| l == "RELAUNCHED LEFT"),
        "the first pane shows its scrollback and its program's output, the exact argument intact: {left:?}"
    );
    assert!(right.contains("SAVED-CONTENT-RIGHT"), "{right:?}");
    assert!(
        !left.contains("SAVED-CONTENT-RIGHT") && !right.contains("LEFT"),
        "nothing lands in the other pane: {left:?} {right:?}"
    );
    assert_eq!(GEN.to_string(), "7");
}
