//! Executes a [`RestorePlan`] over an existing `tmux-control` connection
//! (DESIGN.md §6, §9). The only place in this crate that touches a live
//! connection — everything upstream ([`crate::plan`]) is pure.

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
    pub source: TmuxError,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "restore command #{} failed: {}",
            self.command_index, self.source
        )
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
        let line = cmd.to_command_line().map_err(|err| ApplyError {
            command_index: index,
            source: err.into(),
        })?;
        match client.execute(&line) {
            Ok(_) => outcome.executed += 1,
            Err(err) if is_benign_move_window_failure(cmd, &err) => {
                outcome.skipped_move_window += 1;
            }
            Err(source) => {
                return Err(ApplyError {
                    command_index: index,
                    source,
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

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{SessionName, WindowIndex};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;
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
}
