//! Live: `apply()` driven through a real `tmux-control::Client` against a
//! real, isolated tmux server — the actual mechanism (not the raw-shell
//! stand-in `tests/plan_apply.rs` uses to validate the command strings
//! themselves).

mod support;
use support::{line, linked, snapshot, tree, IsolatedTmux};

use phoenix_core::{
    Content, ContentFailure, Cwd, Foreground, HistoryIndicator, NonEmpty, Pane, PaneId, PaneIndex,
};
use phoenix_restore::{apply, plan, PlanStep, TmuxCommand};

fn captured(
    history_size: u64,
    history_bytes: u64,
    scrollback: Vec<String>,
    visible: Vec<String>,
) -> Content {
    Content::Captured {
        indicator: HistoryIndicator {
            history_size,
            history_bytes,
        },
        scrollback,
        visible,
    }
}

fn pane(index: u32, cwd: &str) -> Pane {
    Pane {
        id: PaneId(index),
        index: PaneIndex(index),
        cwd: Cwd::parse(cwd),
        foreground: Foreground::Shell,
        content: Content::NotCaptured {
            reason: ContentFailure::NotRecorded,
        },
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
    let window = linked(
        0,
        "shell",
        "c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]",
        panes,
        1,
    );
    let session = tree("restored-live", NonEmpty::singleton(window), 0);
    let snapshot = snapshot(vec![session]);

    let restore_plan = plan(&snapshot);
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
    let mut client = harness.connect();

    let base = std::env::temp_dir().join(format!(
        "phoenix-restore-apply-content-live-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();

    let mut pane_a = pane(0, base.to_str().unwrap());
    pane_a.content = captured(1, 1, vec!["DISTINCTIVE-CONTENT-PANE-A".to_string()], vec![]);
    let mut pane_b = pane(1, base.to_str().unwrap());
    pane_b.content = captured(2, 2, vec!["DISTINCTIVE-CONTENT-PANE-B".to_string()], vec![]);

    let panes = NonEmpty::new(pane_a, vec![pane_b]);
    let window = linked(
        0,
        "shell",
        "c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]",
        panes,
        1,
    );
    let session = tree("restored-content", NonEmpty::singleton(window), 0);
    let snapshot = snapshot(vec![session]);

    let restore_plan = plan(&snapshot);
    apply(&mut client, &restore_plan).expect("apply failed");

    // Give the shells a moment to actually run their `cat` before capturing.
    let mut found_a = false;
    let mut found_b = false;
    for _ in 0..30 {
        let panes_out = client
            .execute(&line(
                "list-panes",
                ["-t", "restored-content:0", "-F", "#{pane_index}"],
            ))
            .unwrap();
        let indices: Vec<String> = panes_out
            .lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();

        let mut all_text = String::new();
        for idx in &indices {
            let out = client
                .execute(&line(
                    "capture-pane",
                    ["-p", "-t", &format!("restored-content:0.{idx}")],
                ))
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
        .execute(&line("kill-session", ["-t", "restored-content"]))
        .expect("cleanup kill-session failed");
    client.close();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn apply_relaunches_each_pane_captured_program() {
    let harness = IsolatedTmux::new("restore-apply-relaunch");
    let mut client = harness.connect();

    let base = std::env::temp_dir().join(format!(
        "phoenix-restore-apply-relaunch-live-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();

    let mut pane_a = pane(0, base.to_str().unwrap());
    pane_a.foreground = Foreground::Program {
        argv: NonEmpty::new(
            "echo".to_string(),
            vec!["DISTINCTIVE-RELAUNCH-MARKER".to_string()],
        ),
    };
    let pane_b = pane(1, base.to_str().unwrap());

    let panes = NonEmpty::new(pane_a, vec![pane_b]);
    let window = linked(
        0,
        "shell",
        "c195,80x24,0,0[80x12,0,0,0,80x11,0,13,1]",
        panes,
        0,
    );
    let session = tree("restored-relaunch", NonEmpty::singleton(window), 0);
    let snapshot = snapshot(vec![session]);

    // Pane 0 captured a real foreground program; pane 1 was idle at its
    // shell (`pane()`'s absent argv), so only pane 0 contributes a relaunch.
    let restore_plan = plan(&snapshot);
    assert_eq!(
        restore_plan
            .commands
            .iter()
            .filter(|c| { matches!(c, PlanStep::Command(TmuxCommand::RelaunchProgram { .. })) })
            .count(),
        1,
        "only the pane with a captured foreground program relaunches"
    );
    apply(&mut client, &restore_plan).expect("apply failed");

    // The relaunched pane (snapshot index 0) was also the *active* one, so
    // `panes_active_last` splits it last — its target-side pane index isn't
    // necessarily 0 (this crate never targets panes by index at all, see
    // `panes_active_last`'s doc comment). Enumerate live pane indices
    // instead of assuming one, same as the content-replay test above.
    let mut found_marker = false;
    for _ in 0..30 {
        let panes_out = client
            .execute(&line(
                "list-panes",
                ["-t", "restored-relaunch:0", "-F", "#{pane_index}"],
            ))
            .unwrap();
        let indices: Vec<String> = panes_out
            .lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();

        let mut all_text = String::new();
        for idx in &indices {
            let out = client
                .execute(&line(
                    "capture-pane",
                    ["-p", "-t", &format!("restored-relaunch:0.{idx}")],
                ))
                .unwrap();
            for line in &out.lines {
                all_text.push_str(&String::from_utf8_lossy(line));
                all_text.push('\n');
            }
        }
        if all_text.contains("DISTINCTIVE-RELAUNCH-MARKER") {
            found_marker = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        found_marker,
        "the pane's captured program should have actually run"
    );

    client
        .execute(&line("kill-session", ["-t", "restored-relaunch"]))
        .expect("cleanup kill-session failed");
    client.close();
    let _ = std::fs::remove_dir_all(&base);
}
