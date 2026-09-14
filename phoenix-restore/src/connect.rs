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
//! Every restore entry point calls this, so the bootstrap dance has exactly
//! one implementation (`[LAW:single-enforcer]`) instead of one per caller
//! that would drift.
//!
//! **Verified live, and surprising enough to be worth recording:**
//! - Bare `tmux -C` (no session target) never attaches to an existing
//!   session — it unconditionally creates a new one. So the session count has
//!   to be taken out-of-band with a plain `list-sessions` ([`count_sessions`])
//!   before any control-mode connection opens.
//! - Killing the session a control-mode client is attached to ends that
//!   client's connection (`%exit`). So the bootstrap session can't be torn
//!   down from the connection attached to it; this reconnects onto a real
//!   restored session first, then kills the bootstrap from outside.
//! - A structure subscription observes only the attached session's windows,
//!   not the whole server — so reconnecting onto a real restored session,
//!   rather than leaving the client parked on the bootstrap, matters for any
//!   caller that keeps the client, not just for cleanliness.

use std::process::Command;

use phoenix_core::Snapshot;
use tmux_control::{socket_args, Client, SpawnOptions, SpawnTransport, TmuxError};

use crate::apply::{apply, ApplyError, ApplyOutcome};
use crate::plan::RestorePlan;

/// The throwaway session name used to give control mode something to attach
/// to on an otherwise-empty server. Exported so every caller that reasons
/// about the bootstrap names the same session (`[LAW:one-source-of-truth]`).
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
    /// The restore failed *and* removing the bootstrap session afterwards
    /// failed too, so the server is left with scaffolding on it. Both are
    /// reported: hiding either would misdescribe the server's state.
    TeardownAfterFailure {
        primary: Box<ConnectApplyError>,
        teardown: Box<ConnectApplyError>,
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
            ConnectApplyError::TeardownAfterFailure { primary, teardown } => write!(
                f,
                "{primary}; and the {BOOTSTRAP_SESSION} session it bootstrapped is still \
                 on the server because cleanup also failed: {teardown}"
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
/// effect, so it can't be used for this check). `0` covers both "server isn't
/// running" and "running with no sessions" identically — exactly the cases
/// that need bootstrapping — so callers never have to tell them apart. The one
/// implementation of this query (`[LAW:one-source-of-truth]`).
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
    // Restore issues commands and reads replies; the server's unsolicited
    // notifications and pane output have no consumer here, so both drop.
    Client::connect(transport, drop, |_, _| {}).map_err(ConnectApplyError::Connect)
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
    // Whatever restore_over_bootstrap does, the session created above is
    // removed by the code that created it: on success from a connection
    // already parked elsewhere, on failure from wherever it got to.
    let restored = restore_over_bootstrap(socket.clone(), snapshot, plan);
    let teardown = run_plain(
        socket.as_deref(),
        &["kill-session", "-t", BOOTSTRAP_SESSION],
        "remove the temporary bootstrap session",
    );
    match (restored, teardown) {
        (Ok(restored), Ok(())) => Ok(restored),
        (Ok(_), Err(teardown)) => Err(teardown),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(teardown)) => Err(ConnectApplyError::TeardownAfterFailure {
            primary: Box::new(primary),
            teardown: Box::new(teardown),
        }),
    }
}

/// Attach to the bootstrap session, apply, then reconnect onto a real
/// restored session: killing the session you are attached to ends your own
/// connection, so the caller's teardown has to happen from a connection
/// parked somewhere else. On failure the returned client (if any) is
/// dropped with the error, so the teardown's kill of the bootstrap session
/// ends nothing the caller still holds.
fn restore_over_bootstrap(
    socket: Option<String>,
    snapshot: &Snapshot,
    plan: &RestorePlan,
) -> Result<(Client<SpawnTransport>, ApplyOutcome), ConnectApplyError> {
    let mut client = attach(socket.clone(), Some(BOOTSTRAP_SESSION))?;
    let outcome = apply(&mut client, plan).map_err(ConnectApplyError::Apply)?;

    let target = snapshot.sessions.first().name().as_str().to_string();
    let fresh = SpawnTransport::spawn(&["attach-session", "-t", &target], &spawn_options(socket))
        .map_err(ConnectApplyError::Spawn)?;
    client
        .reconnect(fresh, 0)
        .map_err(ConnectApplyError::Connect)?;
    Ok((client, outcome))
}
