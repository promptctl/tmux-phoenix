//! Executes a [`RestorePlan`] over an existing `tmux-control` connection
//! (DESIGN.md §6, §9). The only place in this crate that touches a live
//! connection — or the local filesystem, for [`PlanStep::ReplayContent`]
//! — everything upstream ([`crate::plan`]) is pure.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tmux_control::{Client, CommandLine, TmuxError, Transport};

use crate::command::{shell_quote, PlanStep, TmuxCommand};
use crate::plan::RestorePlan;

#[derive(Debug)]
pub struct ApplyOutcome {
    /// How many commands ran (including a tolerated `MoveWindow` "same
    /// index" case — see below).
    pub executed: usize,
    /// `MoveWindow` commands that hit tmux's benign "same index" error
    /// (`TmuxCommand::MoveWindow`'s doc comment: the window already
    /// happened to land on the captured index) — not failures, just
    /// no-ops, counted separately for visibility.
    pub skipped_move_window: usize,
}

#[derive(Debug)]
pub struct ApplyError {
    /// Which command in `plan.commands` failed (0-indexed) — needed
    /// because a partially-applied restore leaves the server in a
    /// known-partial state the caller needs to be able to report.
    pub command_index: usize,
    pub source: ApplyErrorSource,
}

#[derive(Debug)]
pub enum ApplyErrorSource {
    Tmux(TmuxError),
    /// [`PlanStep::ReplayContent`] couldn't write its temp file — a
    /// local filesystem failure, not a tmux protocol one.
    TempFile(io::Error),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            ApplyErrorSource::Tmux(e) => {
                write!(f, "restore command #{} failed: {e}", self.command_index)
            }
            ApplyErrorSource::TempFile(e) => write!(
                f,
                "restore command #{} failed to write its content-replay temp file: {e}",
                self.command_index
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Runs every command in `plan`, in order, over `client`. Stops at the
/// first failure that isn't the documented benign `MoveWindow` case —
/// DESIGN.md §9's "every external call checked" — leaving the server
/// partially restored rather than pretending success or silently
/// continuing past a real error.
pub fn apply<T: Transport>(
    client: &mut Client<T>,
    plan: &RestorePlan,
) -> Result<ApplyOutcome, ApplyError> {
    let mut outcome = ApplyOutcome {
        executed: 0,
        skipped_move_window: 0,
    };

    for (index, step) in plan.commands.iter().enumerate() {
        let line = resolve_command_line(step).map_err(|source| ApplyError {
            command_index: index,
            source,
        })?;

        match client.execute(&line) {
            Ok(_) => outcome.executed += 1,
            Err(err) if is_benign_move_window_failure(step, &err) => {
                outcome.skipped_move_window += 1;
            }
            Err(source) => {
                return Err(ApplyError {
                    command_index: index,
                    source: ApplyErrorSource::Tmux(source),
                });
            }
        }
    }

    Ok(outcome)
}

fn is_benign_move_window_failure(step: &PlanStep, err: &TmuxError) -> bool {
    if !matches!(step, PlanStep::Command(TmuxCommand::MoveWindow { .. })) {
        return false;
    }
    let TmuxError::Command { lines, .. } = err else {
        return false;
    };
    lines
        .iter()
        .any(|line| String::from_utf8_lossy(line).contains("same index"))
}

static REPLAY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The command line a step actually sends. A [`PlanStep::Command`] already
/// is one; [`PlanStep::ReplayContent`] becomes one only here, because it
/// writes its content to a temp file first and sends a `cat` of that file
/// into the target pane. The shell reads the file asynchronously after
/// `send-keys` returns, so the only party that knows when it has been
/// consumed is that shell: the keystrokes remove the file right after
/// reading it.
fn resolve_command_line(step: &PlanStep) -> Result<CommandLine, ApplyErrorSource> {
    match step {
        PlanStep::ReplayContent {
            session,
            window,
            lines,
        } => {
            let path = write_replay_temp_file(lines).map_err(ApplyErrorSource::TempFile)?;
            let target = format!("{}:{}", session.as_str(), window.0);
            let quoted = shell_quote(&path.to_string_lossy());
            let shell_command = format!("cat {quoted}; rm -f {quoted}");
            CommandLine::new("send-keys", ["-t", &target, &shell_command, "Enter"])
                .map_err(|e| ApplyErrorSource::Tmux(e.into()))
        }
        PlanStep::Command(cmd) => cmd
            .to_command_line()
            .map_err(|e| ApplyErrorSource::Tmux(e.into())),
    }
}

/// Captured scrollback can hold anything the user saw in a terminal, and the
/// temp dir is shared: the file is readable by its owner only, and the open
/// is exclusive so a path another user pre-placed (a symlink, say) fails
/// loudly instead of being written through. The name carries the clock as
/// well as pid and counter so it is not guessable from `ps` alone.
fn write_replay_temp_file(lines: &[String]) -> io::Result<PathBuf> {
    let n = REPLAY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "phoenix-restore-replay-{}-{nanos}-{n}",
        std::process::id()
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    for line in lines {
        writeln!(f, "{line}")?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{SessionName, WindowIndex};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use tmux_control::CommandLine;

    #[derive(Clone, Default)]
    struct MockState {
        sent: Rc<RefCell<Vec<String>>>,
    }

    struct MockTransport {
        replies: VecDeque<Vec<u8>>,
        state: MockState,
    }

    impl MockTransport {
        fn new(replies: Vec<&str>) -> (Self, MockState) {
            let state = MockState::default();
            let transport = Self {
                replies: replies.into_iter().map(|r| r.as_bytes().to_vec()).collect(),
                state: state.clone(),
            };
            (transport, state)
        }
    }

    impl Transport for MockTransport {
        fn send(&mut self, line: &CommandLine) -> io::Result<()> {
            self.state.sent.borrow_mut().push(line.as_str().to_string());
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.replies.pop_front() {
                Some(chunk) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                None => Ok(0),
            }
        }

        fn close(&mut self) {}
    }

    fn move_window_plan() -> RestorePlan {
        RestorePlan {
            commands: vec![PlanStep::Command(TmuxCommand::MoveWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(0),
            })],
        }
    }

    #[test]
    fn a_benign_same_index_move_window_failure_is_tolerated() {
        let (transport, state) =
            MockTransport::new(vec!["%begin 1 1 1\nsame index: 0\n%error 1 1 1\n"]);
        let mut client = Client::new(transport, drop, |_, _| {});

        let outcome = apply(&mut client, &move_window_plan()).unwrap();
        assert_eq!(outcome.executed, 0);
        assert_eq!(outcome.skipped_move_window, 1);
        assert_eq!(*state.sent.borrow(), vec!["move-window -s main -t main:0"]);
    }

    #[test]
    fn a_different_move_window_failure_is_a_real_error() {
        let (transport, _state) = MockTransport::new(vec![
            "%begin 1 1 1\ncan't find session: main\n%error 1 1 1\n",
        ]);
        let mut client = Client::new(transport, drop, |_, _| {});

        let err = apply(&mut client, &move_window_plan()).unwrap_err();
        assert_eq!(err.command_index, 0);
        assert!(matches!(err.source, ApplyErrorSource::Tmux(_)));
    }

    #[test]
    fn a_successful_command_counts_as_executed() {
        let (transport, _state) = MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n"]);
        let mut client = Client::new(transport, drop, |_, _| {});

        let outcome = apply(&mut client, &move_window_plan()).unwrap();
        assert_eq!(outcome.executed, 1);
        assert_eq!(outcome.skipped_move_window, 0);
    }

    #[test]
    fn a_non_move_window_failure_is_never_treated_as_benign() {
        let plan = RestorePlan {
            commands: vec![PlanStep::Command(TmuxCommand::SelectWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(0),
            })],
        };
        let (transport, _state) =
            MockTransport::new(vec!["%begin 1 1 1\nsame index: 0\n%error 1 1 1\n"]);
        let mut client = Client::new(transport, drop, |_, _| {});

        let err = apply(&mut client, &plan).unwrap_err();
        assert_eq!(err.command_index, 0);
    }

    #[test]
    fn stops_at_the_first_real_failure_and_reports_its_index() {
        let plan = RestorePlan {
            commands: vec![
                PlanStep::Command(TmuxCommand::MoveWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                }),
                PlanStep::Command(TmuxCommand::SelectWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                }),
            ],
        };
        let (transport, _state) = MockTransport::new(vec![
            "%begin 1 1 1\n%end 1 1 1\n",
            "%begin 2 2 2\ncan't find window\n%error 2 2 2\n",
        ]);
        let mut client = Client::new(transport, drop, |_, _| {});

        let err = apply(&mut client, &plan).unwrap_err();
        assert_eq!(err.command_index, 1);
    }

    #[test]
    fn replay_content_sends_a_send_keys_cat_command() {
        let plan = RestorePlan {
            commands: vec![PlanStep::ReplayContent {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(2),
                lines: vec!["hello".to_string(), "world".to_string()],
            }],
        };
        let (transport, state) = MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n"]);
        let mut client = Client::new(transport, drop, |_, _| {});

        apply(&mut client, &plan).unwrap();

        let sent = state.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("send-keys -t main:2 "));
        // The keystrokes are one argument to tmux, and shell-quoted within
        // it, because a shell — not tmux — parses what gets typed.
        assert!(sent[0].contains("\"cat '"));
        assert!(sent[0].contains("; rm -f '"));
        assert!(sent[0].ends_with(" Enter"));
    }

    #[test]
    fn write_replay_temp_file_is_owner_only_and_refuses_an_existing_path() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_replay_temp_file(&["x".to_string()]).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // An exclusive open cannot be redirected through something already
        // sitting at the path.
        let err = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_replay_temp_file_writes_one_line_per_entry() {
        let path = write_replay_temp_file(&["hello".to_string(), "world".to_string()]).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "hello\nworld\n");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_replay_temp_file_paths_are_unique_across_calls() {
        let a = write_replay_temp_file(&["x".to_string()]).unwrap();
        let b = write_replay_temp_file(&["y".to_string()]).unwrap();
        assert_ne!(a, b);
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
    }
}
