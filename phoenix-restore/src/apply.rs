//! Runs a [`Plan`] over whatever executes tmux commands (ARCHITECTURE.md
//! §8). The only place in this crate that touches a connection — or the
//! local filesystem, for [`Step::ReplayContent`]. Each step that creates a
//! window or pane binds its reference to the id tmux reports, and every
//! later step addresses that id, so nothing here depends on what tmux
//! considers current.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use phoenix_core::{Utf8PathBuf, WindowIndex};
use tmux_control::commands::{move_window, new_session, new_window, split_window, switch_client};
use tmux_control::{CommandLine, Execute, PaneId, Target, TmuxError, WindowId};

use crate::plan::Plan;
use crate::step::{relaunch_line, shell_quote, LinkSource, OptionScope, PaneRef, Step, WindowRef};

#[derive(Debug)]
pub struct ApplyError {
    /// Which step of the plan failed (0-indexed). Every step before it ran,
    /// so the server is partially restored; a window whose group this cut
    /// short carries no stamp, so the next plan leaves it standing as a
    /// window that is not the snapshot's and builds the saved one whole.
    pub step: usize,
    pub source: ApplyErrorSource,
}

#[derive(Debug)]
pub enum ApplyErrorSource {
    Tmux(TmuxError),
    /// [`Step::ReplayContent`] couldn't write its temp file — a local
    /// filesystem failure, not a tmux one.
    TempFile(io::Error),
}

impl From<TmuxError> for ApplyErrorSource {
    fn from(err: TmuxError) -> Self {
        ApplyErrorSource::Tmux(err)
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            ApplyErrorSource::Tmux(e) => write!(f, "restore step #{} failed: {e}", self.step),
            ApplyErrorSource::TempFile(e) => write!(
                f,
                "restore step #{} failed to write its content-replay temp file: {e}",
                self.step
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Runs every step of `plan`, in order, over `client`, stopping at the first
/// failure rather than continuing past it.
pub fn apply<C: Execute>(client: &mut C, plan: &Plan) -> Result<(), ApplyError> {
    let mut bound = Bound::default();
    plan.steps()
        .iter()
        .enumerate()
        .try_for_each(|(step, action)| {
            run(client, &mut bound, action).map_err(|source| ApplyError { step, source })
        })
}

/// The ids tmux gave what the plan has created so far.
#[derive(Default)]
struct Bound {
    windows: HashMap<WindowRef, WindowId>,
    panes: HashMap<PaneRef, PaneId>,
}

impl Bound {
    fn window(&self, window: WindowRef) -> WindowId {
        *self
            .windows
            .get(&window)
            .expect("`plan` defines every window reference before a step uses it")
    }

    fn pane(&self, pane: PaneRef) -> PaneId {
        *self
            .panes
            .get(&pane)
            .expect("`plan` defines every pane reference before a step uses it")
    }
}

fn index(index: WindowIndex) -> tmux_control::WindowIndex {
    tmux_control::WindowIndex(index.0)
}

fn path(cwd: &Option<Utf8PathBuf>) -> Option<&str> {
    cwd.as_ref().map(Utf8PathBuf::as_str)
}

fn send<C: Execute>(
    client: &mut C,
    name: &'static str,
    args: &[&str],
) -> Result<Vec<Vec<u8>>, ApplyErrorSource> {
    let line = CommandLine::new(name, args).map_err(TmuxError::from)?;
    Ok(client.execute(&line)?.lines)
}

fn run<C: Execute>(client: &mut C, bound: &mut Bound, step: &Step) -> Result<(), ApplyErrorSource> {
    match step {
        Step::CreateSession {
            name,
            window_name,
            cwd,
            window,
            pane,
        } => {
            let made = new_session(
                client,
                name,
                window_name.as_ref().map(|n| n.as_str()),
                path(cwd),
            )?;
            bound.windows.insert(*window, made.window);
            bound.panes.insert(*pane, made.pane);
        }
        Step::MoveWindow {
            window,
            session,
            to,
        } => {
            // Either answer leaves the window at `to`.
            move_window(client, bound.window(*window), session, index(*to))?;
        }
        Step::NewWindow {
            session,
            index: at,
            name,
            cwd,
            window,
            pane,
        } => {
            let made = new_window(client, session, index(*at), Some(name.as_str()), path(cwd))?;
            bound.windows.insert(*window, made.window);
            bound.panes.insert(*pane, made.pane);
        }
        Step::LinkWindow {
            source,
            into,
            index: at,
            replacing,
        } => {
            let source = Target::WindowId(match source {
                LinkSource::Built(window) => bound.window(*window),
                LinkSource::Live(window) => WindowId(window.0),
            })
            .to_string();
            let target = Target::Window(into.clone(), index(*at)).to_string();
            // `-k` removes the window the link lands on.
            let args: Vec<&str> = replacing
                .iter()
                .map(|_| "-k")
                .chain(["-d", "-s", &source, "-t", &target])
                .collect();
            send(client, "link-window", &args)?;
        }
        Step::SplitPane { from, cwd, pane } => {
            let made = split_window(client, bound.pane(*from), path(cwd))?;
            bound.panes.insert(*pane, made);
        }
        Step::SelectLayout { window, layout } => {
            let target = Target::WindowId(bound.window(*window)).to_string();
            send(client, "select-layout", &["-t", &target, layout.as_str()])?;
        }
        Step::ReplayContent { pane, lines } => {
            // The pane's shell reads the file after `send-keys` returns, so
            // only that shell knows when it has been consumed: the same
            // keystrokes remove it.
            let file = write_replay_temp_file(lines).map_err(ApplyErrorSource::TempFile)?;
            let quoted = shell_quote(&file.to_string_lossy());
            let typed = type_line(
                client,
                bound.pane(*pane),
                &format!("cat {quoted}; rm -f {quoted}"),
            );
            // Nothing was typed, so no shell will remove it. The step
            // reports the typing failure; a removal that fails too has
            // nothing to add to it.
            if typed.is_err() {
                let _ = fs::remove_file(&file);
            }
            typed?;
        }
        Step::Relaunch { pane, argv } => {
            type_line(client, bound.pane(*pane), &relaunch_line(argv))?;
        }
        Step::SelectPane { pane } => {
            let target = Target::Pane(bound.pane(*pane)).to_string();
            send(client, "select-pane", &["-t", &target])?;
        }
        Step::SelectWindow { session, index: at } => {
            let target = Target::Window(session.clone(), index(*at)).to_string();
            send(client, "select-window", &["-t", &target])?;
        }
        Step::SetOption { scope, key, value } => {
            let scope: Vec<String> = match scope {
                OptionScope::Server => vec!["-s".to_owned()],
                OptionScope::Session(session) => {
                    vec![
                        "-t".to_owned(),
                        Target::Session(session.clone()).to_string(),
                    ]
                }
                OptionScope::Window(window) => vec![
                    "-w".to_owned(),
                    "-t".to_owned(),
                    Target::WindowId(bound.window(*window)).to_string(),
                ],
            };
            let args: Vec<&str> = scope
                .iter()
                .map(String::as_str)
                .chain([*key, value.as_str()])
                .collect();
            send(client, "set-option", &args)?;
        }
        Step::SwitchClients { from, to } => {
            const FORMAT: &str = "#{client_name}";
            let from = Target::Session(from.clone()).to_string();
            let listed = send(client, "list-clients", &["-t", &from, "-F", FORMAT])?;
            let clients: Vec<String> = listed
                .iter()
                .map(|name| String::from_utf8(name.clone()))
                .collect::<Result<_, _>>()
                .map_err(|_| TmuxError::UnexpectedReply {
                    expected: FORMAT,
                    output: listed.clone(),
                })?;
            // Either answer leaves the client off `from`.
            for client_name in &clients {
                switch_client(client, client_name, to)?;
            }
        }
        Step::KillSession { name } => {
            let target = Target::Session(name.clone()).to_string();
            send(client, "kill-session", &["-t", &target])?;
        }
    }
    Ok(())
}

/// Types `line` and Enter into `pane`'s shell.
fn type_line<C: Execute>(client: &mut C, pane: PaneId, line: &str) -> Result<(), ApplyErrorSource> {
    let target = Target::Pane(pane).to_string();
    send(client, "send-keys", &["-t", &target, line, "Enter"]).map(drop)
}

static REPLAY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Saved scrollback can hold anything the user saw in a terminal, and the
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
    use crate::plan::{plan, Onto};
    use phoenix_core::{
        Content, ContentFailure, Cwd, Foreground, GenerationId, Layout, Made, NonEmpty,
        OffsetDateTime, Origin, Pane, PaneIndex, Session, Snapshot, TmuxVersion, Touched, WinLink,
        Window, WindowName,
    };
    use std::collections::VecDeque;
    use tmux_control::{CommandOutput, Guard, SessionName};

    const GUARD: Guard = Guard {
        timestamp: 0,
        command_number: 0,
        flags: 1,
    };

    /// Answers each command with the next scripted reply — `Ok` lines for
    /// `%end`, `Err` lines for `%error` — and records what was sent.
    struct Scripted {
        replies: VecDeque<Result<Vec<&'static str>, &'static str>>,
        sent: Vec<String>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<Vec<&'static str>, &'static str>>) -> Self {
            Self {
                replies: replies.into(),
                sent: Vec::new(),
            }
        }
    }

    impl Execute for Scripted {
        fn execute(&mut self, command: &CommandLine) -> Result<CommandOutput, TmuxError> {
            self.sent.push(command.as_str().to_owned());
            let bytes = |lines: Vec<&str>| lines.iter().map(|l| l.as_bytes().to_vec()).collect();
            match self.replies.pop_front().expect("a reply for every command") {
                Ok(lines) => Ok(CommandOutput {
                    guard: GUARD,
                    lines: bytes(lines),
                }),
                Err(line) => Err(TmuxError::Command {
                    guard: GUARD,
                    lines: bytes(vec![line]),
                }),
            }
        }
    }

    /// Session `main` linking one two-pane window, saved as `@3`, at index
    /// 2, its second pane active and running `vim`.
    fn snapshot() -> Snapshot {
        let pane = |index: u32, foreground| Pane {
            id: phoenix_core::PaneId(index),
            index: PaneIndex(index),
            cwd: Cwd::parse(format!("/p{index}")),
            foreground,
            content: Content::NotCaptured {
                reason: ContentFailure::NotRecorded,
            },
        };
        let vim = Foreground::Program {
            argv: NonEmpty::singleton("vim".to_owned()),
        };
        let window = Window::new(
            phoenix_core::WindowId(3),
            Made::NotByPhoenix,
            WindowName::parse("editor").unwrap(),
            Layout::parse("layout").unwrap(),
            false,
            NonEmpty::new(pane(0, Foreground::Shell), vec![pane(1, vim)]),
            PaneIndex(1),
        )
        .unwrap();
        let session = Session::new(
            phoenix_core::SessionName::parse("main").unwrap(),
            None,
            NonEmpty::singleton(WinLink {
                index: WindowIndex(2),
                window: phoenix_core::WindowId(3),
            }),
            WindowIndex(2),
            None,
        )
        .unwrap();
        Snapshot::new(
            Origin::BeforeOriginWasRecorded,
            Touched::Never,
            OffsetDateTime::from_unix_timestamp(1_700_000_000),
            TmuxVersion { major: 3, minor: 7 },
            NonEmpty::singleton(window),
            NonEmpty::singleton(session),
            vec![],
        )
        .unwrap()
    }

    fn onto_no_server() -> Plan {
        let scratch = SessionName::parse("scratch").unwrap();
        plan(
            GenerationId(7),
            &snapshot(),
            Onto::NoServer { scratch: &scratch },
        )
    }

    #[test]
    fn every_step_addresses_the_ids_tmux_reported_for_what_the_plan_made() {
        let mut tmux = Scripted::new(vec![
            Ok(vec!["$1 @5 %9"]),
            // The window landed on its saved index by itself.
            Err("same index: 2"),
            Ok(vec!["client-1", "/dev/ttys004"]),
            Ok(vec![]),
            // The terminal closed after it was listed.
            Err("can't find client: /dev/ttys004"),
            Ok(vec![]),
            Ok(vec!["%10"]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
        ]);

        apply(&mut tmux, &onto_no_server()).expect("apply");

        assert_eq!(
            tmux.sent,
            [
                r##"new-session -d -s main -n editor -c /p0 -P -F "#{session_id} #{window_id} #{pane_id}""##,
                "move-window -d -s @5 -t =main:=2",
                r##"list-clients -t =scratch: -F "#{client_name}""##,
                "switch-client -c client-1 -t =main:",
                "switch-client -c /dev/ttys004 -t =main:",
                "kill-session -t =scratch:",
                r##"split-window -d -t "%9" -c /p1 -P -F "#{pane_id}""##,
                "select-layout -t @5 tiled",
                "select-layout -t @5 layout",
                r#"send-keys -t "%10" "'vim'" Enter"#,
                r#"select-pane -t "%10""#,
                "set-option -w -t @5 @phoenix-window 7:@3",
                "select-window -t =main:=2",
                "set-option -t =main: @phoenix-restored 7",
                "set-option -s @phoenix-generation 7",
            ]
        );
    }

    #[test]
    fn the_first_failure_stops_the_plan_and_names_its_step() {
        let mut tmux = Scripted::new(vec![
            Ok(vec!["$1 @5 %9"]),
            Ok(vec![]),
            Ok(vec![]),
            Ok(vec![]),
            Err("no space for new pane"),
        ]);

        let err = apply(&mut tmux, &onto_no_server()).unwrap_err();

        assert_eq!(err.step, 4);
        assert!(matches!(
            err.source,
            ApplyErrorSource::Tmux(TmuxError::Command { .. })
        ));
        assert_eq!(tmux.sent.len(), 5, "nothing runs past the failure");
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
