//! Boot restore (DESIGN.md §8): on start, restore `latest` into a server that
//! holds nothing the user built — no sessions at all, or only the bootstrap
//! sessions a login terminal creates by starting `tmux` before the daemon
//! connects — and never into a server holding a session the user built.
//!
//! How to restore into either kind of server (counting sessions out of band,
//! setting scaffolding aside, reattaching, moving terminals over) belongs to
//! `phoenix_restore::connect_and_apply`, which the CLI's `restore` shares.
//! This module only decides whether to call it.

use phoenix_restore::{connect_and_apply, plan, probe, ConnectApplyError, ServerState};
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
    /// Probing the server, or restoring into it, failed; carries
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
/// holds nothing the user built. Returns `Ok(None)` when the server has no
/// sessions and nothing is saved: there is nothing to attach to and nothing
/// to put there, and starting a server the user never asked for is not the
/// daemon's call, so the caller waits and asks again (DESIGN.md §8: "if tmux
/// isn't running … the daemon idles").
///
/// `on_notification` becomes the returned client's notification sink.
/// `on_log` receives one line saying which branch was taken, naming the
/// user's sessions when those are why nothing was restored.
///
/// This probe only decides; `connect_and_apply` probes again right before it
/// acts, so a session the user builds in between turns the server into one
/// that restore only adds beside, never one it replaces.
pub fn connect_and_boot(
    socket: Option<String>,
    store: &Store,
    mut on_log: impl FnMut(&str),
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Option<Client<SpawnTransport>>, BootError> {
    let server = probe(socket.as_deref()).map_err(BootError::Restore)?;
    if let ServerState::Built(built) = &server {
        let names: Vec<&str> = built.iter().map(|name| name.as_str()).collect();
        on_log(&format!(
            "server has session(s) the user built ({}); not restoring into it, staying in save mode",
            names.join(", ")
        ));
        return attach(socket, on_notification).map(Some);
    }

    match (server, store.load_latest()) {
        (_, Ok(snapshot)) => {
            let (client, _outcome) =
                connect_and_apply(socket, &snapshot, &plan(&snapshot), on_notification)
                    .map_err(BootError::Restore)?;
            on_log(&format!(
                "restored {} session(s) from the latest snapshot",
                snapshot.sessions.len()
            ));
            Ok(Some(client))
        }
        (ServerState::Empty, Err(StoreError::NoLatest)) => {
            on_log("no sessions and no saved snapshot; waiting for a tmux session");
            Ok(None)
        }
        (_, Err(StoreError::NoLatest)) => {
            on_log(
                "server holds only bootstrap sessions and nothing is saved yet; staying in save mode",
            );
            attach(socket, on_notification).map(Some)
        }
        (_, Err(e)) => Err(BootError::Store(e)),
    }
}

/// Bare `attach-session` attaches to the server's most recently used
/// session, which exists: the caller just probed it.
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
