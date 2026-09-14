//! Boot restore (DESIGN.md §8): "On start, if the server has only the
//! default empty session, apply `latest`; if sessions already exist, log and
//! stay in save mode — never clobber a live server."
//!
//! The empty-server mechanics (counting sessions out of band, bootstrapping
//! something to attach to, tearing it down from a connection parked
//! elsewhere) belong to `phoenix_restore::connect_and_apply`, which the CLI's
//! `restore` shares. This module only decides whether to call it.

use phoenix_restore::{connect_and_apply, count_sessions, plan, ConnectApplyError};
use phoenix_store::{Store, StoreError};
use tmux_control::{Client, ServerMessage, SpawnOptions, SpawnTransport, TmuxError};

#[derive(Debug)]
pub enum BootError {
    /// Spawning the `tmux -C` child for a live server failed at the OS level.
    Spawn(std::io::Error),
    /// The control-mode handshake with a live server failed.
    Connect(TmuxError),
    /// `latest` exists but could not be read. Refused rather than treated as
    /// "nothing saved": waiting forever on an empty server would hide it.
    Store(StoreError),
    /// Counting sessions, or restoring onto the empty server, failed; carries
    /// `phoenix-restore`'s own located message.
    Restore(ConnectApplyError),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Spawn(e) => write!(f, "failed to spawn tmux: {e}"),
            BootError::Connect(e) => write!(f, "failed to connect to tmux: {e}"),
            BootError::Store(e) => write!(f, "failed to load the latest snapshot: {e}"),
            BootError::Restore(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

/// Connects for the daemon's run, restoring `latest` first when the server
/// has no sessions. Returns `Ok(None)` when the server has no sessions and
/// nothing is saved: there is nothing to attach to and nothing to put there,
/// and starting a server the user never asked for is not the daemon's call,
/// so the caller waits and asks again (DESIGN.md §8: "if tmux isn't running
/// … the daemon idles").
///
/// `on_notification` becomes the returned client's notification sink.
/// `on_log` receives one line saying which branch was taken.
pub fn connect_and_boot(
    socket: Option<String>,
    store: &Store,
    mut on_log: impl FnMut(&str),
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Option<Client<SpawnTransport>>, BootError> {
    let existing = count_sessions(socket.as_deref()).map_err(BootError::Restore)?;
    if existing > 0 {
        on_log(&format!(
            "server already has {existing} session(s); staying in save mode"
        ));
        return attach(socket, on_notification).map(Some);
    }

    let snapshot = match store.load_latest() {
        Ok(snapshot) => snapshot,
        Err(StoreError::NoLatest) => {
            on_log("no sessions and no saved snapshot; waiting for a tmux session");
            return Ok(None);
        }
        Err(e) => return Err(BootError::Store(e)),
    };
    let (client, _outcome) =
        connect_and_apply(socket, &snapshot, &plan(&snapshot), on_notification)
            .map_err(BootError::Restore)?;
    on_log(&format!(
        "restored {} session(s) from the latest snapshot",
        snapshot.sessions.len()
    ));
    Ok(Some(client))
}

/// Bare `attach-session` attaches to the server's most recently used
/// session, which exists: the caller just counted it.
fn attach(
    socket: Option<String>,
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Client<SpawnTransport>, BootError> {
    let options = SpawnOptions {
        socket,
        ..Default::default()
    };
    let transport =
        SpawnTransport::spawn(&["attach-session"], &options).map_err(BootError::Spawn)?;
    Client::connect(transport, on_notification, |_, _| {}).map_err(BootError::Connect)
}
