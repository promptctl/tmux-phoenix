//! The one connection strategy every restore uses (tmux-parity-ure.1).
//!
//! A control-mode client attaches to a *session* — so restoring onto a server
//! that has no sessions (empty, or not even running yet) has nothing to attach
//! to, and a bare `attach-session` there dies at EOF with the cryptic
//! `TmuxError::TransportClosed` ("transport closed before the command's reply
//! arrived"). This module removes that failure mode for good by making the
//! connection a function of one fact — *does the server already have a
//! session?* — with two arms:
//!
//! - **Populated**: attach to the server's existing session, apply, done. No
//!   bootstrap, so an already-running server behaves exactly as before.
//! - **Empty / not running**: create a throwaway `phoenix-boot` session (which
//!   also starts the server if it was down), attach to it, apply the plan
//!   (that recreates the snapshot's real sessions), reconnect onto a real
//!   restored session, then kill the now-unattached bootstrap — leaving no
//!   scaffolding behind.
//!
//! Both the CLI `restore` and the daemon's boot restore call this, so the
//! bootstrap dance has exactly one implementation (`[LAW:single-enforcer]`)
//! instead of one per caller that would drift. The three verified-live
//! mechanics that make the empty case work — bare `tmux -C` always creates a
//! new session so the count must be taken out-of-band first; you can't kill
//! the session you're attached to without ending your own connection; a
//! structure subscription only sees the attached session — are documented in
//! `phoenix-daemon`'s `boot.rs`, the original home of this logic.

use std::process::Command;

use phoenix_core::Snapshot;
use tmux_control::{socket_args, Client, SpawnOptions, SpawnTransport, TmuxError};

use crate::apply::{apply, ApplyError, ApplyOutcome};
use crate::plan::RestorePlan;

/// The throwaway session name used to give control mode something to attach
/// to on an otherwise-empty server. Exported so `phoenix-daemon`'s other boot
/// branches name the same session (`[LAW:one-source-of-truth]`).
pub const BOOTSTRAP_SESSION: &str = "phoenix-boot";

#[derive(Debug)]
pub enum ConnectApplyError {
    /// Spawning the `tmux -C` control-mode child failed at the OS level.
    Spawn(std::io::Error),
    /// The control-mode handshake — initial or the post-restore reconnect —
    /// failed.
    Connect(TmuxError),
    /// A restore command failed while applying the plan; the server is left
    /// partially restored (see [`ApplyError`]).
    Apply(ApplyError),
    /// A plain (non-control-mode) `tmux` helper — creating or removing the
    /// bootstrap session — failed. `action` names what was being attempted so
    /// the message points at the actual problem rather than a generic
    /// transport error (tmux-parity-ure.1 acceptance criterion 5).
    Tmux {
        action: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for ConnectApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectApplyError::Spawn(e) => write!(f, "failed to spawn tmux: {e}"),
            ConnectApplyError::Connect(e) => write!(f, "failed to connect to tmux: {e}"),
            ConnectApplyError::Apply(e) => write!(f, "{e}"),
            ConnectApplyError::Tmux { action, detail } => {
                write!(f, "failed to {action}: {detail}")
            }
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
/// effect, so it can't be used for this check). `0` covers both "server isn't
/// running" and "running with no sessions" identically — exactly the cases
/// that need bootstrapping — so callers never have to tell them apart. The one
/// implementation of this query (`[LAW:one-source-of-truth]`), shared with
/// `phoenix-daemon`'s boot decision.
pub fn count_sessions(socket: Option<&str>) -> usize {
    let mut cmd = Command::new("tmux");
    cmd.args(socket_args(socket));
    cmd.args(["list-sessions", "-F", "#{session_name}"]);
    match cmd.output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        _ => 0,
    }
}

/// Run a plain `tmux` command (no `-C`) and hold it to account: an OS failure
/// or a non-zero exit becomes an error naming `action` and carrying tmux's own
/// stderr (`[LAW:no-silent-failure]` — the bootstrap create/teardown are not
/// allowed to fail quietly and leave the restore in a half-known state).
fn run_plain(
    socket: Option<&str>,
    args: &[&str],
    action: &'static str,
) -> Result<(), ConnectApplyError> {
    let mut cmd = Command::new("tmux");
    cmd.args(socket_args(socket));
    cmd.args(args);
    let out = cmd.output().map_err(|e| ConnectApplyError::Tmux {
        action,
        detail: e.to_string(),
    })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        format!("tmux exited with {}", out.status)
    } else {
        stderr
    };
    Err(ConnectApplyError::Tmux { action, detail })
}

fn attach(
    socket: Option<String>,
    target: Option<&str>,
) -> Result<Client<SpawnTransport>, ConnectApplyError> {
    let mut args = vec!["attach-session"];
    if let Some(t) = target {
        args.push("-t");
        args.push(t);
    }
    let transport =
        SpawnTransport::spawn(&args, &spawn_options(socket)).map_err(ConnectApplyError::Spawn)?;
    Client::connect(transport).map_err(ConnectApplyError::Connect)
}

/// Connect to the server on `socket` and apply `plan`, bootstrapping a
/// throwaway session first if the server has none so restore works uniformly
/// whether the server is empty, freshly started, or already populated. On
/// success the returned client is attached to a real restored (or
/// pre-existing) session — never the bootstrap one — and no scaffolding
/// remains on the server.
///
/// `snapshot` is the same one `plan` was built from: its first session names
/// the reconnect target for the bootstrap teardown, so a caller can't hand
/// over a target that the plan doesn't actually create.
pub fn connect_and_apply(
    socket: Option<String>,
    snapshot: &Snapshot,
    plan: &RestorePlan,
) -> Result<(Client<SpawnTransport>, ApplyOutcome), ConnectApplyError> {
    // [LAW:dataflow-not-control-flow] the single irreducible fork: an empty
    // server genuinely needs different *effects* (create + tear down a
    // bootstrap session) than a populated one, and the discriminator is one
    // out-of-band fact taken before any connection exists.
    if count_sessions(socket.as_deref()) > 0 {
        let mut client = attach(socket, None)?;
        let outcome = apply(&mut client, plan).map_err(ConnectApplyError::Apply)?;
        return Ok((client, outcome));
    }

    run_plain(
        socket.as_deref(),
        &["new-session", "-d", "-s", BOOTSTRAP_SESSION],
        "create the temporary bootstrap session",
    )?;
    let mut client = attach(socket.clone(), Some(BOOTSTRAP_SESSION))?;
    let outcome = apply(&mut client, plan).map_err(ConnectApplyError::Apply)?;

    // Reconnect onto a real restored session before killing the bootstrap:
    // killing the session you're attached to ends your own connection, so the
    // teardown has to happen from a connection parked somewhere else first.
    let target = snapshot.sessions.first().name().as_str().to_string();
    let fresh = SpawnTransport::spawn(
        &["attach-session", "-t", &target],
        &spawn_options(socket.clone()),
    )
    .map_err(ConnectApplyError::Spawn)?;
    client
        .reconnect(fresh, 0)
        .map_err(ConnectApplyError::Connect)?;

    run_plain(
        socket.as_deref(),
        &["kill-session", "-t", BOOTSTRAP_SESSION],
        "remove the temporary bootstrap session",
    )?;

    Ok((client, outcome))
}
