//! Executes a [`RestorePlan`] over an existing `tmux-control` connection
//! (DESIGN.md §6, §9). The only place in this crate that touches a live
//! connection — or the local filesystem, for [`TmuxCommand::ReplayContent`]
//! — everything upstream ([`crate::plan`]) is pure.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tmux_control::{Client, TmuxError, Transport};

use crate::command::TmuxCommand;
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
    /// [`TmuxCommand::ReplayContent`] couldn't write its temp file — a
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

    for (index, cmd) in plan.commands.iter().enumerate() {
        let line = resolve_command_line(cmd).map_err(|source| ApplyError {
            command_index: index,
            source,
        })?;

        match client.execute(&line) {
            Ok(_) => outcome.executed += 1,
            Err(err) if is_benign_move_window_failure(cmd, &err) => {
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

fn is_benign_move_window_failure(cmd: &TmuxCommand, err: &TmuxError) -> bool {
    if !matches!(cmd, TmuxCommand::MoveWindow { .. }) {
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

/// Every command except `ReplayContent` renders directly via
/// `to_command_string` — see [`TmuxCommand::ReplayContent`]'s doc comment
/// for why that one needs special handling here instead: it writes its
/// content to a temp file (not deleted afterward — the shell reads it
/// asynchronously after `send-keys` returns, so there's no safe point to
/// clean it up from here; left for the OS's own temp-dir hygiene) and sends
/// a `cat` of that file into the target pane.
fn resolve_command_string(cmd: &TmuxCommand) -> io::Result<String> {
    match cmd {
        TmuxCommand::ReplayContent {
            session,
            window,
            lines,
        } => {
            let path = write_replay_temp_file(lines)?;
            let shell_command = format!(
                "cat {}",
                tmux_control::commands::tmux_escape(&path.to_string_lossy())
            );
            let target = format!("{}:{}", session.as_str(), window.0);
            Ok(format!(
                "send-keys -t {} {} Enter",
                tmux_control::commands::tmux_escape(&target),
                tmux_control::commands::tmux_escape(&shell_command)
            ))
        }
        other => Ok(other.to_command_string()),
    }
}

fn write_replay_temp_file(lines: &[String]) -> io::Result<PathBuf> {
    let n = REPLAY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("phoenix-restore-replay-{}-{n}", std::process::id()));
    let mut f = fs::File::create(&path)?;
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
            commands: vec![TmuxCommand::MoveWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(0),
            }],
        }
    }

    #[test]
    fn a_benign_same_index_move_window_failure_is_tolerated() {
        let (transport, state) =
            MockTransport::new(vec!["%begin 1 1 1\nsame index: 0\n%error 1 1 1\n"]);
        let mut client = Client::new(transport);

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
        let mut client = Client::new(transport);

        let err = apply(&mut client, &move_window_plan()).unwrap_err();
        assert_eq!(err.command_index, 0);
        assert!(matches!(err.source, ApplyErrorSource::Tmux(_)));
    }

    #[test]
    fn a_successful_command_counts_as_executed() {
        let (transport, _state) = MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n"]);
        let mut client = Client::new(transport);

        let outcome = apply(&mut client, &move_window_plan()).unwrap();
        assert_eq!(outcome.executed, 1);
        assert_eq!(outcome.skipped_move_window, 0);
    }

    #[test]
    fn a_non_move_window_failure_is_never_treated_as_benign() {
        let plan = RestorePlan {
            commands: vec![TmuxCommand::SelectWindow {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(0),
            }],
        };
        let (transport, _state) =
            MockTransport::new(vec!["%begin 1 1 1\nsame index: 0\n%error 1 1 1\n"]);
        let mut client = Client::new(transport);

        let err = apply(&mut client, &plan).unwrap_err();
        assert_eq!(err.command_index, 0);
    }

    #[test]
    fn stops_at_the_first_real_failure_and_reports_its_index() {
        let plan = RestorePlan {
            commands: vec![
                TmuxCommand::MoveWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                },
                TmuxCommand::SelectWindow {
                    session: SessionName::parse("main").unwrap(),
                    window: WindowIndex(0),
                },
            ],
        };
        let (transport, _state) = MockTransport::new(vec![
            "%begin 1 1 1\n%end 1 1 1\n",
            "%begin 2 2 2\ncan't find window\n%error 2 2 2\n",
        ]);
        let mut client = Client::new(transport);

        let err = apply(&mut client, &plan).unwrap_err();
        assert_eq!(err.command_index, 1);
    }

    #[test]
    fn replay_content_sends_a_send_keys_cat_command() {
        let plan = RestorePlan {
            commands: vec![TmuxCommand::ReplayContent {
                session: SessionName::parse("main").unwrap(),
                window: WindowIndex(2),
                lines: vec!["hello".to_string(), "world".to_string()],
            }],
        };
        let (transport, state) = MockTransport::new(vec!["%begin 1 1 1\n%end 1 1 1\n"]);
        let mut client = Client::new(transport);

        apply(&mut client, &plan).unwrap();

        let sent = state.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].starts_with("send-keys -t 'main:2' "));
        assert!(sent[0].contains("cat"));
        assert!(sent[0].ends_with(" Enter"));
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
