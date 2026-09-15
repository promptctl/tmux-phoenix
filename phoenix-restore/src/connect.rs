//! The one connection strategy every restore uses (tmux-parity-ure.1,
//! tmux-parity-ure.j0f).
//!
//! A control-mode client attaches to a *session*, and a restore creates
//! sessions whose names may already belong to a session holding nothing of
//! the user's — the one a login terminal made by starting `tmux` first. So
//! the strategy is a function of what the server holds ([`probe`]):
//!
//! - **Empty / not running**: nothing to attach to.
//! - **Only bootstrap sessions** ([`phoenix_core::Session::is_bootstrap`]):
//!   nothing worth keeping, but names the snapshot may need, and possibly a
//!   terminal attached.
//! - **Built**: at least one session the user built; restore only adds.
//!
//! Every case runs the same steps over a list of *scaffolding* sessions, and
//! only that list differs (`[LAW:dataflow-not-control-flow]`): set the
//! scaffolding aside (a session created on an empty server, bootstrap
//! sessions renamed out of the snapshot's way, nothing on a built server),
//! apply over it, reattach onto a restored session, then move every client
//! still on scaffolding onto that session and remove the scaffolding. A
//! restore that fails before its plan applied puts the scaffolding back
//! instead, so a renamed login session gets its name back rather than being
//! killed with a terminal still attached.
//!
//! Every restore entry point calls this, so the dance has exactly one
//! implementation (`[LAW:single-enforcer]`) instead of one per caller that
//! would drift.
//!
//! **Verified live, and surprising enough to be worth recording:**
//! - Bare `tmux -C` (no session target) never attaches to an existing
//!   session — it unconditionally creates a new one. So the session count has
//!   to be taken out-of-band with a plain `list-sessions` ([`count_sessions`])
//!   before any control-mode connection opens.
//! - Killing the session a control-mode client is attached to ends that
//!   client's connection (`%exit`). So scaffolding can't be torn down from the
//!   connection attached to it; this reconnects onto a real restored session
//!   first, then removes the scaffolding from outside.
//! - Renaming a session keeps its clients attached, and `switch-client -c`
//!   moves a terminal client onto another session, after which killing the
//!   session it left leaves the terminal running.
//! - A structure subscription observes only the attached session's windows,
//!   not the whole server — so reconnecting onto a real restored session,
//!   rather than leaving the client parked on scaffolding, matters for any
//!   caller that keeps the client, not just for cleanliness.

use std::collections::HashSet;
use std::process::Command;

use phoenix_capture::{capture, CaptureError, ContentCapture};
use phoenix_core::{NonEmpty, SessionName, Snapshot};
use tmux_control::{socket_args, Client, ServerMessage, SpawnOptions, SpawnTransport, TmuxError};

use crate::apply::{apply, ApplyError, ApplyOutcome};
use crate::plan::RestorePlan;

/// Scaffolding sessions are named this plus a number chosen free of every
/// name on the server and in the snapshot, so no snapshot can collide with
/// the scaffolding its own restore needs.
const SCAFFOLD_PREFIX: &str = "phoenix-boot-";

/// What the target server holds, as far as restoring into it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// No server running, or one with no sessions.
    Empty,
    /// Every session is a bootstrap session; restore replaces them.
    BootstrapOnly(NonEmpty<SessionName>),
    /// The sessions the user built; restore only ever adds beside them.
    Built(NonEmpty<SessionName>),
}

impl ServerState {
    fn of(live: &Snapshot) -> Self {
        let built: Vec<SessionName> = live
            .sessions
            .iter()
            .filter(|session| !session.is_bootstrap())
            .map(|session| session.name().clone())
            .collect();
        match NonEmpty::from_vec(built) {
            Some(built) => ServerState::Built(built),
            None => ServerState::BootstrapOnly(NonEmpty::new(
                live.sessions.first().name().clone(),
                live.sessions
                    .iter()
                    .skip(1)
                    .map(|session| session.name().clone())
                    .collect(),
            )),
        }
    }
}

#[derive(Debug)]
pub enum ConnectApplyError {
    /// Spawning the `tmux -C` control-mode child failed at the OS level.
    Spawn(std::io::Error),
    /// The control-mode handshake — probe, initial, or the post-restore
    /// reconnect — failed.
    Connect(TmuxError),
    /// Capturing the live server, to learn what restoring into it means,
    /// failed.
    Probe(CaptureError),
    /// A restore command failed while applying the plan; the server is left
    /// partially restored (see [`ApplyError`]).
    Apply(ApplyError),
    /// A plain (non-control-mode) `tmux` helper — setting scaffolding aside,
    /// moving a client, or removing scaffolding — failed. `action` names what
    /// was being attempted so the message points at the actual problem rather
    /// than a generic transport error (tmux-parity-ure.1 acceptance
    /// criterion 5).
    Tmux {
        action: &'static str,
        detail: String,
    },
    /// The restore failed *and* putting its scaffolding back failed too, so
    /// the server is left holding `phoenix-boot-*` sessions. Both are
    /// reported: hiding either would misdescribe the server's state.
    TeardownAfterFailure {
        primary: Box<ConnectApplyError>,
        teardown: Box<ConnectApplyError>,
    },
    /// The plan applied in full, so the snapshot's sessions exist on the
    /// server, but reattaching to a restored session afterwards failed, so
    /// there is no client to hand back. Carries the outcome because the
    /// restore itself succeeded: reporting it as a plain failure would invite
    /// a re-run that collides with the sessions it already created.
    Reattach {
        outcome: ApplyOutcome,
        source: Box<ConnectApplyError>,
    },
}

impl std::fmt::Display for ConnectApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectApplyError::Spawn(e) => write!(f, "failed to spawn tmux: {e}"),
            ConnectApplyError::Connect(e) => write!(f, "failed to connect to tmux: {e}"),
            ConnectApplyError::Probe(e) => {
                write!(
                    f,
                    "failed to read what the server holds before restoring: {e}"
                )
            }
            ConnectApplyError::Apply(e) => write!(f, "{e}"),
            ConnectApplyError::Tmux { action, detail } => {
                write!(f, "failed to {action}: {detail}")
            }
            ConnectApplyError::TeardownAfterFailure { primary, teardown } => write!(
                f,
                "{primary}; and putting the {SCAFFOLD_PREFIX}* sessions it set aside back \
                 also failed, so they are still on the server: {teardown}"
            ),
            ConnectApplyError::Reattach { outcome, source } => write!(
                f,
                "restored the snapshot ({} commands applied) but could not reattach \
                 to it afterwards: {source}",
                outcome.executed
            ),
        }
    }
}

impl std::error::Error for ConnectApplyError {}

fn spawn_options(socket: Option<String>) -> SpawnOptions {
    SpawnOptions {
        socket,
        ..Default::default()
    }
}

/// How many sessions the server on `socket` currently has. A plain,
/// non-control-mode `list-sessions` — it doesn't attach and doesn't create
/// anything (unlike a bare `tmux -C`, which always spawns a session as a side
/// effect, so it can't be used for this check). `Ok(0)` covers both "server
/// isn't running" and "running with no sessions". Every other failure is an
/// error, never a zero: a failed query against a populated server must not
/// send restore down the empty-server path (`[LAW:no-silent-failure]`). The
/// one implementation of this query (`[LAW:one-source-of-truth]`).
pub fn count_sessions(socket: Option<&str>) -> Result<usize, ConnectApplyError> {
    let listing = match run_plain(
        socket,
        &["list-sessions", "-F", "#{session_name}"],
        "count the server's sessions",
    ) {
        Err(ConnectApplyError::Tmux { detail, .. }) if reports_no_server(&detail) => return Ok(0),
        other => other?,
    };
    Ok(listing.lines().filter(|l| !l.is_empty()).count())
}

/// What the server on `socket` holds. An empty server is answered out of band
/// ([`count_sessions`]); a populated one is captured over a short-lived
/// control connection, because telling a bootstrap session from a built one
/// needs each pane's foreground program.
///
/// The answer describes a server nothing locks: a session the user builds
/// right after it is taken is still on the server when a restore acts. That
/// is why [`connect_and_apply`] probes again immediately before acting rather
/// than trusting a caller's earlier probe.
pub fn probe(socket: Option<&str>) -> Result<ServerState, ConnectApplyError> {
    if count_sessions(socket)? == 0 {
        return Ok(ServerState::Empty);
    }
    let mut client = attach(socket.map(str::to_string), None, drop)?;
    let live = capture(&mut client, ContentCapture::Off);
    client.close();
    Ok(ServerState::of(&live.map_err(ConnectApplyError::Probe)?))
}

/// tmux's own stderr for a failed plain invocation, or the exit status when
/// it printed nothing.
fn failure_detail(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("tmux exited with {}", out.status)
    } else {
        stderr
    }
}

/// Whether a failed `list-sessions`'s stderr says there is no server — as
/// opposed to a server that exists but couldn't be queried. Verified against
/// tmux 3.6a, the two absent shapes are:
///
/// | stderr | meaning | absent? |
/// |---|---|---|
/// | `no server running on <path>` | socket missing its server, or stale | yes |
/// | `error connecting to <path> (No such file or directory)` | no socket file | yes |
/// | `error connecting to <path> (Socket operation on non-socket)` | path is not a socket | no |
/// | `error connecting to <path> (Permission denied)` | someone else's server | no |
/// | anything else, or empty | unknown | no |
fn reports_no_server(stderr: &str) -> bool {
    stderr.starts_with("no server running on ")
        || (stderr.starts_with("error connecting to ")
            && stderr.ends_with(" (No such file or directory)"))
}

/// Run a plain `tmux` command (no `-C`) and hold it to account: an OS failure
/// or a non-zero exit becomes an error naming `action` and carrying tmux's own
/// stderr (`[LAW:no-silent-failure]` — no scaffolding step is allowed to fail
/// quietly and leave the restore in a half-known state). Returns stdout.
fn run_plain(
    socket: Option<&str>,
    args: &[&str],
    action: &'static str,
) -> Result<String, ConnectApplyError> {
    let mut cmd = Command::new("tmux");
    cmd.args(socket_args(socket));
    cmd.args(args);
    let out = cmd.output().map_err(|e| ConnectApplyError::Tmux {
        action,
        detail: e.to_string(),
    })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(ConnectApplyError::Tmux {
        action,
        detail: failure_detail(&out),
    })
}

/// A target naming exactly the session `name`, never a prefix match. Only
/// for commands that take a session target: a pane or window target needs a
/// trailing colon (`=name:`), or tmux reports it cannot find the pane.
fn exact(name: &str) -> String {
    format!("={name}")
}

fn attach(
    socket: Option<String>,
    target: Option<&str>,
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Client<SpawnTransport>, ConnectApplyError> {
    let mut args = vec!["attach-session"];
    if let Some(t) = target {
        args.push("-t");
        args.push(t);
    }
    let transport =
        SpawnTransport::spawn(&args, &spawn_options(socket)).map_err(ConnectApplyError::Spawn)?;
    // Pane output has no consumer in any restore caller, so it drops; what
    // happens to notifications is the caller's, since a client's sinks are
    // fixed for its whole life, reconnects included.
    Client::connect(transport, on_notification, |_, _| {}).map_err(ConnectApplyError::Connect)
}

/// One scaffolding session, recording how it was set aside, which decides how
/// a failed restore puts it back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scaffold {
    /// Created on an empty server so control mode has something to attach to.
    Created { name: String },
    /// A bootstrap session renamed out of the snapshot's way.
    Renamed { from: String, to: String },
}

impl Scaffold {
    fn name(&self) -> &str {
        match self {
            Scaffold::Created { name } | Scaffold::Renamed { to: name, .. } => name,
        }
    }

    fn set_aside(&self, socket: Option<&str>) -> Result<(), ConnectApplyError> {
        match self {
            Scaffold::Created { name } => run_plain(
                socket,
                &["new-session", "-d", "-s", name],
                "create the temporary bootstrap session",
            ),
            Scaffold::Renamed { from, to } => run_plain(
                socket,
                &["rename-session", "-t", &exact(from), to],
                "rename a bootstrap session out of the snapshot's way",
            ),
        }
        .map(drop)
    }

    /// Undoes [`Scaffold::set_aside`] after a restore whose plan did not apply.
    fn put_back(&self, socket: Option<&str>) -> Result<(), ConnectApplyError> {
        match self {
            Scaffold::Created { name } => kill_session(socket, name),
            Scaffold::Renamed { from, to } => run_plain(
                socket,
                &["rename-session", "-t", &exact(to), from],
                "give a bootstrap session its name back",
            )
            .map(drop),
        }
    }

    /// Removes this scaffolding once the snapshot is restored, first moving any
    /// client still on it — a login terminal — onto `restored`, since a
    /// client whose session is killed is detached.
    fn remove(&self, socket: Option<&str>, restored: &str) -> Result<(), ConnectApplyError> {
        let clients = run_plain(
            socket,
            &[
                "list-clients",
                "-t",
                &exact(self.name()),
                "-F",
                "#{client_name}",
            ],
            "list the clients on a bootstrap session",
        )?;
        for client in clients.lines().filter(|l| !l.is_empty()) {
            run_plain(
                socket,
                &["switch-client", "-c", client, "-t", &exact(restored)],
                "move a terminal onto the restored session",
            )?;
        }
        kill_session(socket, self.name())
    }
}

fn kill_session(socket: Option<&str>, name: &str) -> Result<(), ConnectApplyError> {
    run_plain(
        socket,
        &["kill-session", "-t", &exact(name)],
        "remove a temporary bootstrap session",
    )
    .map(drop)
}

/// The scaffolding restoring `snapshot` into `server` needs (pure). Each
/// name is the lowest free `phoenix-boot-N`, free of the snapshot's names,
/// the server's, and the ones already picked.
fn scaffolding(server: &ServerState, snapshot: &Snapshot) -> Vec<Scaffold> {
    let mut taken: HashSet<String> = snapshot
        .sessions
        .iter()
        .map(|session| session.name().as_str().to_string())
        .collect();
    let fresh = |taken: &mut HashSet<String>| {
        let name = (0u64..)
            .map(|n| format!("{SCAFFOLD_PREFIX}{n}"))
            .find(|candidate| !taken.contains(candidate))
            .expect("an unbounded range always holds a free name");
        taken.insert(name.clone());
        name
    };
    match server {
        ServerState::Empty => vec![Scaffold::Created {
            name: fresh(&mut taken),
        }],
        ServerState::BootstrapOnly(names) => {
            taken.extend(names.iter().map(|name| name.as_str().to_string()));
            names
                .iter()
                .map(|name| Scaffold::Renamed {
                    from: name.as_str().to_string(),
                    to: fresh(&mut taken),
                })
                .collect()
        }
        ServerState::Built(_) => Vec::new(),
    }
}

/// Runs `step` over every scaffold even after one fails, so one stuck session
/// doesn't strand the rest, and reports the first failure.
fn each(
    scaffolding: &[Scaffold],
    step: impl Fn(&Scaffold) -> Result<(), ConnectApplyError>,
) -> Result<(), ConnectApplyError> {
    scaffolding.iter().map(step).fold(Ok(()), Result::and)
}

/// `primary` with whatever cleanup ran after it folded in.
fn with_cleanup(
    primary: ConnectApplyError,
    cleanup: Result<(), ConnectApplyError>,
) -> ConnectApplyError {
    match cleanup {
        Ok(()) => primary,
        Err(teardown) => ConnectApplyError::TeardownAfterFailure {
            primary: Box::new(primary),
            teardown: Box::new(teardown),
        },
    }
}

/// Connect to the server on `socket` and apply `plan`, replacing the
/// server's sessions only when every one is a bootstrap session, so restore
/// works the same whether the server is empty, holds a login terminal's fresh
/// session, or holds sessions the user built. On success the returned client
/// is attached to a restored session and no scaffolding remains.
///
/// `snapshot` is the same one `plan` was built from: its first session names
/// the reconnect target, so a caller can't hand over a target that the plan
/// doesn't actually create.
///
/// `on_notification` becomes the returned client's notification sink. A
/// caller that keeps the client listening (the daemon) passes its own; one
/// that only restores passes `drop`.
pub fn connect_and_apply(
    socket: Option<String>,
    snapshot: &Snapshot,
    plan: &RestorePlan,
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<(Client<SpawnTransport>, ApplyOutcome), ConnectApplyError> {
    let sock = socket.as_deref();
    let scaffolding = scaffolding(&probe(sock)?, snapshot);

    for (set, scaffold) in scaffolding.iter().enumerate() {
        if let Err(primary) = scaffold.set_aside(sock) {
            let undone = each(&scaffolding[..set], |s| s.put_back(sock));
            return Err(with_cleanup(primary, undone));
        }
    }

    let restored_name = snapshot.sessions.first().name().as_str();
    let attach_to = scaffolding.first().map(|s| exact(s.name()));
    let restored = restore_over(
        socket.clone(),
        attach_to.as_deref(),
        plan,
        restored_name,
        on_notification,
    );

    // A plan that applied in full has replaced the scaffolding even when the
    // reattach after it failed, so only a plan that did not apply puts the
    // scaffolding back.
    match restored {
        Ok(restored) => each(&scaffolding, |s| s.remove(sock, restored_name)).map(|()| restored),
        Err(reattach @ ConnectApplyError::Reattach { .. }) => Err(with_cleanup(
            reattach,
            each(&scaffolding, |s| s.remove(sock, restored_name)),
        )),
        Err(primary) => Err(with_cleanup(
            primary,
            each(&scaffolding, |s| s.put_back(sock)),
        )),
    }
}

/// Attach (onto the first scaffold when there is one), apply, then reconnect
/// onto a real restored session: killing the session you are attached to ends
/// your own connection, so scaffolding has to be removed from a connection
/// parked somewhere else. On failure the returned client (if any) is dropped
/// with the error, so removing scaffolding ends nothing the caller still
/// holds.
fn restore_over(
    socket: Option<String>,
    attach_to: Option<&str>,
    plan: &RestorePlan,
    restored_name: &str,
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<(Client<SpawnTransport>, ApplyOutcome), ConnectApplyError> {
    let mut client = attach(socket.clone(), attach_to, on_notification)?;
    let outcome = apply(&mut client, plan).map_err(ConnectApplyError::Apply)?;

    let reattached = SpawnTransport::spawn(
        &["attach-session", "-t", &exact(restored_name)],
        &spawn_options(socket),
    )
    .map_err(ConnectApplyError::Spawn)
    .and_then(|fresh| {
        client
            .reconnect(fresh, 0)
            .map_err(ConnectApplyError::Connect)
    });
    match reattached {
        Ok(_) => Ok((client, outcome)),
        Err(source) => Err(ConnectApplyError::Reattach {
            outcome,
            source: Box::new(source),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, OffsetDateTime, Pane, PaneId, PaneIndex,
        ProgramName, Session, TmuxVersion, Window, WindowIndex, WindowName,
    };

    fn snapshot_of(names: &[&str]) -> Snapshot {
        let session = |name: &str| {
            let pane = Pane {
                id: PaneId(0),
                index: PaneIndex(0),
                cwd: None,
                program: CapturedProgram {
                    command: ProgramName::parse("zsh").unwrap(),
                    argv: None,
                },
                content: None,
            };
            let window = Window::new(
                WindowIndex(0),
                WindowName::parse("shell").unwrap(),
                Layout::parse("b25d,80x24,0,0,0").unwrap(),
                NonEmpty::singleton(pane),
                PaneIndex(0),
            )
            .unwrap();
            Session::new(
                SessionName::parse(name).unwrap(),
                NonEmpty::singleton(window),
                WindowIndex(0),
            )
            .unwrap()
        };
        Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions: NonEmpty::from_vec(names.iter().map(|n| session(n)).collect()).unwrap(),
        }
    }

    fn names(names: &[&str]) -> NonEmpty<SessionName> {
        NonEmpty::from_vec(
            names
                .iter()
                .map(|n| SessionName::parse(*n).unwrap())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn an_empty_server_gets_one_created_scaffold_named_clear_of_the_snapshot() {
        assert_eq!(
            scaffolding(
                &ServerState::Empty,
                &snapshot_of(&["phoenix-boot-0", "work"])
            ),
            vec![Scaffold::Created {
                name: "phoenix-boot-1".to_string()
            }]
        );
    }

    #[test]
    fn every_bootstrap_session_is_renamed_clear_of_the_snapshot_the_server_and_each_other() {
        assert_eq!(
            scaffolding(
                &ServerState::BootstrapOnly(names(&["0", "phoenix-boot-0"])),
                &snapshot_of(&["0", "work"])
            ),
            vec![
                Scaffold::Renamed {
                    from: "0".to_string(),
                    to: "phoenix-boot-1".to_string()
                },
                Scaffold::Renamed {
                    from: "phoenix-boot-0".to_string(),
                    to: "phoenix-boot-2".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_built_server_needs_no_scaffolding() {
        assert!(scaffolding(
            &ServerState::Built(names(&["mine"])),
            &snapshot_of(&["work"])
        )
        .is_empty());
    }

    #[test]
    fn a_missing_or_stale_socket_reports_no_server() {
        assert!(reports_no_server(
            "no server running on /tmp/tmux-501/default"
        ));
        assert!(reports_no_server(
            "error connecting to /tmp/tmux-501/x (No such file or directory)"
        ));
    }

    #[test]
    fn any_other_list_sessions_failure_is_not_an_empty_server() {
        for stderr in [
            "error connecting to /tmp/x (Socket operation on non-socket)",
            "error connecting to /tmp/x (Permission denied)",
            "server exited unexpectedly",
            "tmux exited with exit status: 1",
        ] {
            assert!(
                !reports_no_server(stderr),
                "{stderr:?} must not read as absent"
            );
        }
    }

    #[test]
    fn counting_sessions_on_a_path_that_is_not_a_socket_fails_loudly() {
        let dir = std::env::temp_dir().join(format!("phoenix-not-a-socket-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = count_sessions(dir.to_str());
        std::fs::remove_dir(&dir).unwrap();
        assert!(
            matches!(result, Err(ConnectApplyError::Tmux { .. })),
            "a directory at the socket path must be an error, got {result:?}"
        );
    }

    #[test]
    fn counting_sessions_where_no_server_runs_is_zero() {
        let socket = std::env::temp_dir().join(format!("phoenix-no-server-{}", std::process::id()));
        assert_eq!(count_sessions(socket.to_str()).unwrap(), 0);
    }
}
