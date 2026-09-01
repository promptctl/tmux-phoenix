//! `plan(&Snapshot) -> RestorePlan` (DESIGN.md §6): pure, no I/O,
//! unit-testable with no tmux running. `tmux-restore-qll.2` executes the
//! resulting ordered [`TmuxCommand`]s.

use phoenix_core::{CapturedProgram, Pane, Session, Snapshot, Window, WindowIndex};

use crate::command::{PlanStep, TmuxCommand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub commands: Vec<PlanStep>,
}

pub fn plan(snapshot: &Snapshot) -> RestorePlan {
    let mut commands = Vec::new();
    for session in snapshot.sessions.iter() {
        plan_session(session, &mut commands);
    }
    RestorePlan { commands }
}

/// `window.panes()` reordered so the window's originally-active pane is
/// last. Neither `split-window` nor anything else lets a plan request or
/// fix up a specific *pane* index (unlike windows — see
/// [`TmuxCommand::MoveWindow`]), so this crate never targets a pane by
/// index at all; instead the active pane is always the *last* one created.
/// Verified live: the most recently split pane is the one tmux leaves
/// active, and a later `select-layout` doesn't change that — so creation
/// order alone is enough to end up with the right pane active, with no
/// `select-pane` needed (DESIGN.md §6 mentions `select-pane` among the
/// commands a plan might use; this restore never needs it).
fn panes_active_last(window: &Window) -> Vec<&Pane> {
    let active = window.active();
    let mut ordered: Vec<&Pane> = window.panes().iter().collect();
    if let Some(pos) = ordered.iter().position(|p| p.index == active) {
        let active_pane = ordered.remove(pos);
        ordered.push(active_pane);
    }
    ordered
}

fn plan_session(session: &Session, commands: &mut Vec<PlanStep>) {
    let mut windows = session.windows().iter();
    let first_window = windows
        .next()
        .expect("NonEmpty<Window> always has a first element");

    // tmux always creates exactly one window with a new session, so the
    // first window is special: its creation and one of its panes' creation
    // are both folded into the one `new-session` call.
    let first_window_panes = panes_active_last(first_window);
    commands.push(PlanStep::Command(TmuxCommand::NewSession {
        session: session.name().clone(),
        first_window_name: first_window.name().clone(),
        cwd: first_window_panes[0].cwd.clone(),
    }));
    // `new-session` has no way to request a specific window index (unlike
    // `new-window`, below) — the window lands wherever the target server's
    // `base-index` puts it, so this relocates it to the captured index
    // before anything else references it by that index. See
    // `TmuxCommand::MoveWindow`'s doc comment.
    commands.push(PlanStep::Command(TmuxCommand::MoveWindow {
        session: session.name().clone(),
        window: first_window.index(),
    }));
    // The implicit first pane is current right now, immediately after
    // creation — the only moment `ReplayContent`/`RelaunchProgram`'s
    // "current pane" targeting can reach it (see their doc comments).
    plan_pane_extras(
        session,
        first_window.index(),
        first_window_panes[0],
        commands,
    );
    plan_remaining_panes(session, first_window, &first_window_panes, commands);

    for window in windows {
        let window_panes = panes_active_last(window);
        commands.push(PlanStep::Command(TmuxCommand::NewWindow {
            session: session.name().clone(),
            window: window.index(),
            name: window.name().clone(),
            cwd: window_panes[0].cwd.clone(),
        }));
        plan_pane_extras(session, window.index(), window_panes[0], commands);
        plan_remaining_panes(session, window, &window_panes, commands);
    }

    commands.push(PlanStep::Command(TmuxCommand::SelectWindow {
        session: session.name().clone(),
        window: session.active(),
    }));
}

/// `ordered_panes` (see [`panes_active_last`]): whichever command created
/// `window` already created `ordered_panes[0]` implicitly, so this only
/// needs `split-window` for the rest.
fn plan_remaining_panes(
    session: &Session,
    window: &Window,
    ordered_panes: &[&Pane],
    commands: &mut Vec<PlanStep>,
) {
    for pane in &ordered_panes[1..] {
        commands.push(PlanStep::Command(TmuxCommand::SplitWindow {
            session: session.name().clone(),
            window: window.index(),
            cwd: pane.cwd.clone(),
        }));
        // Still the current pane of `window` right after this split — see
        // `PlanStep::ReplayContent`'s doc comment for why this can't be
        // deferred to later.
        plan_pane_extras(session, window.index(), pane, commands);
    }

    if ordered_panes.len() > 1 {
        commands.push(PlanStep::Command(TmuxCommand::SelectLayout {
            session: session.name().clone(),
            window: window.index(),
            layout: window.layout().clone(),
        }));
    }
}

/// Everything that targets `pane` as the window's *current* pane, right
/// after it was created: captured scrollback first (so it's visible history
/// by the time the program that produced it gets relaunched on top — same
/// ordering tmux-resurrect used), then that program.
fn plan_pane_extras(
    session: &Session,
    window: WindowIndex,
    pane: &Pane,
    commands: &mut Vec<PlanStep>,
) {
    maybe_replay_content(session, window, pane, commands);
    maybe_relaunch_program(session, window, pane, commands);
}

/// Emits `ReplayContent` for `pane` if (and only if) it has captured
/// content — a pane with `content: None` (structure-only capture, or a
/// degraded pane) simply has no history to replay: whether replay happens
/// is entirely driven by whether the snapshot itself has content to
/// replay.
fn maybe_replay_content(
    session: &Session,
    window: WindowIndex,
    pane: &Pane,
    commands: &mut Vec<PlanStep>,
) {
    if let Some(content) = &pane.content {
        commands.push(PlanStep::ReplayContent {
            session: session.name().clone(),
            window,
            lines: content.scrollback.clone(),
        });
    }
}

/// Basenames (tmux's own `pane_current_command`) of interactive shells —
/// see [`foreground_argv`].
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "csh", "ash", "elvish", "nu", "xonsh",
];

/// The command line to relaunch in `program`'s pane, or `None` when the
/// pane had no foreground program of its own to resume. Two shapes mean
/// "none":
///
/// - Empty `argv`: best-effort `ps` recovery failed for this pane
///   (`CapturedProgram`'s doc comment) — there is no command line to run.
/// - A bare interactive shell: tmux reports the pane's own shell as its
///   foreground whenever the pane is idle at a prompt, and restore already
///   creates each pane as a fresh shell at the captured cwd, so running it
///   again would only nest a second shell inside the first. Only a shell
///   invoked with no non-flag argument counts as idle — `bash deploy.sh`
///   is a script the user was running, not an idle prompt.
fn foreground_argv(program: &CapturedProgram) -> Option<&[String]> {
    let (_, args) = program.argv.split_first()?;
    let idle_shell =
        SHELLS.contains(&program.command.as_str()) && args.iter().all(|arg| arg.starts_with('-'));
    (!idle_shell).then_some(program.argv.as_slice())
}

/// Emits `RelaunchProgram` for whatever `pane` had in its foreground when
/// the snapshot was taken (see [`foreground_argv`]), so a restored pane
/// comes back running what it was running.
fn maybe_relaunch_program(
    session: &Session,
    window: WindowIndex,
    pane: &Pane,
    commands: &mut Vec<PlanStep>,
) {
    if let Some(argv) = foreground_argv(&pane.program) {
        commands.push(PlanStep::Command(TmuxCommand::RelaunchProgram {
            session: session.name().clone(),
            window,
            argv: argv.to_vec(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, PaneContent, PaneId,
        PaneIndex, SessionName, TmuxVersion, WindowIndex, WindowName,
    };

    fn pane(index: u32, cwd: &str) -> Pane {
        Pane {
            index: PaneIndex(index),
            cwd: cwd.into(),
            program: CapturedProgram::new("zsh", vec!["zsh".to_string()]),
            content: None,
        }
    }

    fn window(index: u32, name: &str, panes: NonEmpty<Pane>, active: u32) -> Window {
        Window::new(
            WindowIndex(index),
            WindowName::parse(name).unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            panes,
            PaneIndex(active),
        )
        .unwrap()
    }

    fn snapshot(sessions: NonEmpty<Session>) -> Snapshot {
        Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions,
        }
    }

    #[test]
    fn single_session_single_window_single_pane_is_new_session_plus_move_window() {
        let win = window(0, "shell", NonEmpty::singleton(pane(0, "/home/user")), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        assert_eq!(
            plan.commands,
            vec![
                PlanStep::Command(TmuxCommand::NewSession {
                    session: SessionName::parse("main").unwrap(),
                    first_window_name: WindowName::parse("shell").unwrap(),
                    cwd: "/home/user".into(),
                }),
                PlanStep::Command(TmuxCommand::MoveWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                }),
                PlanStep::Command(TmuxCommand::SelectWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                }),
            ]
        );
    }

    #[test]
    fn extra_panes_get_split_window_and_a_trailing_select_layout() {
        let panes = NonEmpty::from_vec(vec![pane(0, "/a"), pane(1, "/b"), pane(2, "/c")]).unwrap();
        let win = window(0, "shell", panes, 1);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        let splits: Vec<_> = plan
            .commands
            .iter()
            .filter(|c| matches!(c, PlanStep::Command(TmuxCommand::SplitWindow { .. })))
            .collect();
        assert_eq!(
            splits.len(),
            2,
            "one pane came from new-session, two remain"
        );

        let layouts: Vec<_> = plan
            .commands
            .iter()
            .filter(|c| matches!(c, PlanStep::Command(TmuxCommand::SelectLayout { .. })))
            .collect();
        assert_eq!(layouts.len(), 1);
    }

    #[test]
    fn single_pane_windows_get_no_select_layout() {
        let win = window(0, "shell", NonEmpty::singleton(pane(0, "/a")), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));
        assert!(!plan
            .commands
            .iter()
            .any(|c| matches!(c, PlanStep::Command(TmuxCommand::SelectLayout { .. }))));
    }

    #[test]
    fn the_active_pane_is_always_split_last_regardless_of_its_original_position() {
        // Active is pane index 0 — originally *first* — so it must be
        // reordered to be split last, not omitted as "already existing".
        let panes =
            NonEmpty::from_vec(vec![pane(0, "/active"), pane(1, "/b"), pane(2, "/c")]).unwrap();
        let win = window(0, "shell", panes, 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        // new-session must not have swallowed the active pane's cwd as the
        // implicit first pane.
        assert!(matches!(
            &plan.commands[0],
            PlanStep::Command(TmuxCommand::NewSession { cwd, .. }) if cwd.as_str() != "/active"
        ));

        let splits: Vec<_> = plan
            .commands
            .iter()
            .filter_map(|c| match c {
                PlanStep::Command(TmuxCommand::SplitWindow { cwd, .. }) => Some(cwd.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            splits.last(),
            Some(&"/active"),
            "the active pane's split-window must be the last one issued"
        );
    }

    #[test]
    fn a_second_window_gets_new_window_at_its_captured_index() {
        let w0 = window(0, "shell", NonEmpty::singleton(pane(0, "/a")), 0);
        let w1 = window(5, "editor", NonEmpty::singleton(pane(0, "/b")), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::new(w0, vec![w1]),
            WindowIndex(5),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        assert!(plan
            .commands
            .contains(&PlanStep::Command(TmuxCommand::NewWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(5),
                name: WindowName::parse("editor").unwrap(),
                cwd: "/b".into(),
            })));
        assert!(plan
            .commands
            .contains(&PlanStep::Command(TmuxCommand::SelectWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(5),
            })));
    }

    #[test]
    fn a_pane_relaunches_its_captured_program_with_the_exact_captured_argv() {
        let mut p = pane(0, "/proj");
        p.program = CapturedProgram::new("vim", vec!["vim".to_string(), "foo.txt".to_string()]);
        let win = window(0, "editor", NonEmpty::singleton(p), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        let relaunched: Vec<&Vec<String>> = plan
            .commands
            .iter()
            .filter_map(|c| match c {
                PlanStep::Command(TmuxCommand::RelaunchProgram { argv, .. }) => Some(argv),
                _ => None,
            })
            .collect();
        assert_eq!(
            relaunched,
            vec![&vec!["vim".to_string(), "foo.txt".to_string()]]
        );
    }

    #[test]
    fn a_pane_idle_at_its_shell_is_left_as_a_plain_shell() {
        // tmux reports the pane's own shell as the foreground program of an
        // idle pane; restore already creates that shell, so relaunching it
        // would just nest a second one.
        for (command, argv) in [
            ("zsh", vec!["-zsh"]),
            ("zsh", vec!["/bin/zsh", "-l"]),
            ("bash", vec!["bash"]),
            ("fish", vec!["fish"]),
        ] {
            let mut p = pane(0, "/a");
            p.program = CapturedProgram::new(
                command,
                argv.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            );
            let win = window(0, "shell", NonEmpty::singleton(p), 0);
            let session = Session::new(
                SessionName::parse("main").unwrap(),
                NonEmpty::singleton(win),
                WindowIndex(0),
            )
            .unwrap();
            let plan = plan(&snapshot(NonEmpty::singleton(session)));
            assert!(
                !plan
                    .commands
                    .iter()
                    .any(|c| matches!(c, PlanStep::Command(TmuxCommand::RelaunchProgram { .. }))),
                "{command} {argv:?} should not be relaunched"
            );
        }
    }

    #[test]
    fn a_shell_running_a_script_is_a_real_program_and_is_relaunched() {
        let mut p = pane(0, "/a");
        p.program = CapturedProgram::new("bash", vec!["bash".to_string(), "deploy.sh".to_string()]);
        let win = window(0, "shell", NonEmpty::singleton(p), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));
        assert!(plan.commands.iter().any(
            |c| matches!(c, PlanStep::Command(TmuxCommand::RelaunchProgram { argv, .. }) if argv[1] == "deploy.sh")
        ));
    }

    #[test]
    fn a_pane_whose_argv_recovery_failed_is_left_as_a_plain_shell() {
        let mut p = pane(0, "/a");
        p.program = CapturedProgram::new("vim", vec![]);
        let win = window(0, "editor", NonEmpty::singleton(p), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));
        assert!(!plan
            .commands
            .iter()
            .any(|c| matches!(c, PlanStep::Command(TmuxCommand::RelaunchProgram { .. }))));
    }

    #[test]
    fn a_pane_with_no_captured_content_gets_no_replay_command() {
        let win = window(0, "shell", NonEmpty::singleton(pane(0, "/a")), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));
        assert!(!plan
            .commands
            .iter()
            .any(|c| matches!(c, PlanStep::ReplayContent { .. })));
    }

    #[test]
    fn a_pane_with_captured_content_gets_a_replay_command_right_after_its_creation() {
        let mut p = pane(0, "/a");
        p.content = Some(PaneContent::new(
            PaneId(7),
            10,
            512,
            vec!["captured line".to_string()],
            vec!["captured line".to_string()],
        ));
        let win = window(0, "shell", NonEmpty::singleton(p), 0);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        // NewSession creates the one pane; ReplayContent must immediately
        // follow it (before anything else could shift "current" away).
        assert_eq!(plan.commands.len(), 4, "{:?}", plan.commands);
        assert!(matches!(
            plan.commands[0],
            PlanStep::Command(TmuxCommand::NewSession { .. })
        ));
        assert!(matches!(
            plan.commands[1],
            PlanStep::Command(TmuxCommand::MoveWindow { .. })
        ));
        match &plan.commands[2] {
            PlanStep::ReplayContent { lines, .. } => {
                assert_eq!(lines, &vec!["captured line".to_string()])
            }
            other => panic!("expected ReplayContent, got {other:?}"),
        }
    }

    #[test]
    fn each_pane_in_a_multi_pane_window_gets_its_own_content_replayed() {
        let mut p0 = pane(0, "/a");
        p0.content = Some(PaneContent::new(
            PaneId(1),
            1,
            1,
            vec!["from pane a".to_string()],
            vec![],
        ));
        let mut p1 = pane(1, "/b");
        p1.content = Some(PaneContent::new(
            PaneId(2),
            2,
            2,
            vec!["from pane b".to_string()],
            vec![],
        ));
        let panes = NonEmpty::new(p0, vec![p1]);
        // active = 1 (pane b), so panes_active_last reorders pane b to be
        // split *last* — pane a stays first (the implicit new-session pane).
        let win = window(0, "shell", panes, 1);
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::singleton(session)));

        let replayed: Vec<&str> = plan
            .commands
            .iter()
            .filter_map(|c| match c {
                PlanStep::ReplayContent { lines, .. } => Some(lines[0].as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(replayed, vec!["from pane a", "from pane b"]);

        // The second pane's SplitWindow must come before its ReplayContent
        // (current-pane targeting requires the pane to exist first), and
        // that ReplayContent must come before SelectLayout.
        let split_pos = plan
            .commands
            .iter()
            .position(|c| matches!(c, PlanStep::Command(TmuxCommand::SplitWindow { .. })))
            .unwrap();
        let second_replay_pos = plan
            .commands
            .iter()
            .position(
                |c| matches!(c, PlanStep::ReplayContent { lines, .. } if lines[0] == "from pane b"),
            )
            .unwrap();
        let layout_pos = plan
            .commands
            .iter()
            .position(|c| matches!(c, PlanStep::Command(TmuxCommand::SelectLayout { .. })))
            .unwrap();
        assert!(split_pos < second_replay_pos);
        assert!(second_replay_pos < layout_pos);
    }

    #[test]
    fn multiple_sessions_are_each_fully_planned() {
        let w0 = window(0, "shell", NonEmpty::singleton(pane(0, "/a")), 0);
        let s0 = Session::new(
            SessionName::parse("one").unwrap(),
            NonEmpty::singleton(w0),
            WindowIndex(0),
        )
        .unwrap();
        let w1 = window(0, "shell", NonEmpty::singleton(pane(0, "/b")), 0);
        let s1 = Session::new(
            SessionName::parse("two").unwrap(),
            NonEmpty::singleton(w1),
            WindowIndex(0),
        )
        .unwrap();
        let plan = plan(&snapshot(NonEmpty::new(s0, vec![s1])));

        let new_sessions: Vec<_> = plan
            .commands
            .iter()
            .filter(|c| matches!(c, PlanStep::Command(TmuxCommand::NewSession { .. })))
            .collect();
        assert_eq!(new_sessions.len(), 2);
    }
}
