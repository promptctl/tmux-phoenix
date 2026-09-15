//! Boot restore (DESIGN.md §8): on start, restore `latest` into a server that
//! holds nothing the user built — no sessions at all, or only the bootstrap
//! sessions a login terminal creates by starting `tmux` before the daemon
//! connects — and never into a server holding a session the user built.
//!
//! How to restore into either kind of server (counting sessions out of band,
//! setting scaffolding aside, reattaching, moving terminals over) belongs to
//! `phoenix_restore::connect_and_apply`, which the CLI's `restore` shares.
//! This module only decides whether to call it.

use phoenix_core::{NonEmpty, SessionName, Snapshot};
use phoenix_restore::{
    connect_and_apply, plan, probe, server_id, ConnectApplyError, ServerId, ServerState,
};
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
    /// The server had no sessions left by the time boot had connected to it.
    Vanished,
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Spawn(e) => write!(f, "failed to spawn tmux: {e}"),
            BootError::Connect(e) => write!(f, "failed to connect to tmux: {e}"),
            BootError::Store(e) => write!(f, "failed to load the latest snapshot: {e}"),
            BootError::Restore(e) => write!(f, "{e}"),
            BootError::Vanished => write!(
                f,
                "the tmux server had no sessions left by the time the daemon connected"
            ),
        }
    }
}

impl std::error::Error for BootError {}

/// What boot decided, which is what a later refused save means to the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boot {
    /// Restored `latest`, found nothing saved to restore, or saved since.
    /// Final for this server: bootstrap sessions on it later on, this run or
    /// any reconnect to it, are the user's to keep.
    Settled,
    /// Stayed out because the probe took these sessions for ones the user
    /// built. Provisional until the run's first save succeeds: the probe
    /// reads foreground programs at one instant, and a login shell's prompt
    /// briefly runs programs of its own (DESIGN.md §8).
    Declined(NonEmpty<SessionName>),
}

impl Boot {
    /// Whether `refused`, a capture the store refused as bootstrap-only, shows
    /// this boot misread the server: every session it took for built is still
    /// there, and now holds nothing built. A server whose built sessions the
    /// user closed is a different server, not a misreading, and stays theirs.
    /// A session whose program the user quit before the first save, leaving
    /// one idle shell, reads the same as a misreading and is restored over,
    /// on the same terms as a login terminal someone typed into before boot
    /// (DESIGN.md §8).
    pub fn misread(&self, refused: &Snapshot) -> bool {
        match self {
            Boot::Settled => false,
            Boot::Declined(built) => built
                .iter()
                .all(|name| refused.sessions.iter().any(|s| s.name() == name)),
        }
    }
}

/// The boot decision made for one server. A reconnect to that server keeps it
/// rather than booting again, and the run advances it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    /// So a later reconnect can tell this server from one restarted on the
    /// same socket.
    pub server: ServerId,
    pub boot: Boot,
}

/// A connection boot handed back, with the decision it was made under.
pub struct Booted {
    pub client: Client<SpawnTransport>,
    pub decided: Decided,
}

/// Connects for the daemon's run, restoring `latest` first when the server
/// holds nothing the user built. A reconnect to `last`'s server is not a boot:
/// it keeps `last`'s decision, so a server the daemon has already run beside is
/// never restored over by a second look at it. Returns `Ok(None)` when the server has no
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
/// that restore only adds beside, never one it replaces, and reads the server
/// once more before retiring scaffolding, so a login session the user builds
/// in while the plan applies is kept.
pub fn connect_and_boot(
    socket: Option<String>,
    store: &Store,
    last: Option<&Decided>,
    mut on_log: impl FnMut(&str),
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Option<Booted>, BootError> {
    let identify = socket.clone();
    let identify = identify.as_deref();
    let server = probe(identify).map_err(BootError::Restore)?;
    let returning = server_id(identify)
        .map_err(BootError::Restore)?
        .and_then(|id| last.filter(|last| last.server == id));

    let connected = match (server, returning, store.load_latest()) {
        (_, Some(last), _) => {
            on_log(
                "reconnected to the server this daemon was running on; keeping its boot decision, \
                 not restoring into it",
            );
            Some((attach(socket, on_notification)?, last.boot.clone()))
        }
        (ServerState::Built(built), None, _) => {
            on_log(&format!(
                "server has session(s) the user built ({}); not restoring into it, staying in save mode",
                joined(&built)
            ));
            Some((attach(socket, on_notification)?, Boot::Declined(built)))
        }
        (_, None, Ok(snapshot)) => {
            let (client, _outcome) =
                connect_and_apply(socket, &snapshot, &plan(&snapshot), on_notification)
                    .map_err(BootError::Restore)?;
            on_log(&format!(
                "restored {} session(s) from the latest snapshot",
                snapshot.sessions.len()
            ));
            Some((client, Boot::Settled))
        }
        (ServerState::Empty, None, Err(StoreError::NoLatest)) => {
            on_log("no sessions and no saved snapshot; waiting for a tmux session");
            None
        }
        (_, None, Err(StoreError::NoLatest)) => {
            on_log(
                "server holds only bootstrap sessions and nothing is saved yet; staying in save mode",
            );
            Some((attach(socket, on_notification)?, Boot::Settled))
        }
        (_, None, Err(e)) => return Err(BootError::Store(e)),
    };

    connected
        .map(|(client, boot)| {
            let server = server_id(identify)
                .map_err(BootError::Restore)?
                .ok_or(BootError::Vanished)?;
            Ok(Booted {
                client,
                decided: Decided { server, boot },
            })
        })
        .transpose()
}

fn joined(names: &NonEmpty<SessionName>) -> String {
    names
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
