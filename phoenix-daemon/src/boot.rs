//! Boot restore (DESIGN.md §8): on start, restore `latest` into a server that
//! holds nothing the user built — no sessions at all, or only the bootstrap
//! sessions a login terminal creates by starting `tmux` before the daemon
//! connects — and never into a server holding a session the user built.
//!
//! The restore itself is `phoenix-restore`'s plan, which only adds what the
//! server lacks. This module decides whether to run it, by the bootstrap
//! heuristic that `tmux-laws-a4x.9xh` replaces with the server's own
//! `@phoenix-generation` mark; the heuristic's probe lives here, with its
//! one reader, and goes with it.

use phoenix_capture::{capture, CaptureError, Previous};
use phoenix_core::{GenerationId, NonEmpty, Origin, SessionName, Shells, Snapshot};
use phoenix_restore::{apply, plan, scratch_name, ApplyError, Onto, Plan};
use phoenix_store::{Store, StoreError};
use tmux_control::{
    Attach, Client, Connection, ServerMessage, SpawnOptions, SpawnTransport, TmuxError,
};

/// What a server that holds sessions holds, as far as restoring into it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Every session is a bootstrap session ([`Snapshot::is_bootstrap`]).
    BootstrapOnly(NonEmpty<SessionName>),
    /// The sessions the user built.
    Built(NonEmpty<SessionName>),
}

impl ServerState {
    fn of(live: &Snapshot) -> Self {
        let built: Vec<SessionName> = live
            .sessions()
            .iter()
            .filter(|session| !live.is_bootstrap(session))
            .map(|session| session.name().clone())
            .collect();
        match NonEmpty::from_vec(built) {
            Some(built) => ServerState::Built(built),
            None => ServerState::BootstrapOnly(NonEmpty::new(
                live.sessions().first().name().clone(),
                live.sessions()
                    .iter()
                    .skip(1)
                    .map(|session| session.name().clone())
                    .collect(),
            )),
        }
    }
}

#[derive(Debug)]
pub enum BootError {
    /// Spawning the `tmux -C` child for a live server failed at the OS level.
    Spawn(std::io::Error),
    /// Opening a control-mode connection to the server failed.
    Connect(TmuxError),
    /// `latest` exists but could not be read. Refused rather than treated as
    /// "nothing saved": waiting forever on an empty server would hide it.
    Store(StoreError),
    /// Reading what the server holds failed.
    Probe(CaptureError),
    /// A step of the restore plan failed; the server is partially restored.
    Apply(ApplyError),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Spawn(e) => write!(f, "failed to spawn tmux: {e}"),
            BootError::Connect(e) => write!(f, "failed to connect to tmux: {e}"),
            BootError::Store(e) => write!(f, "failed to load the latest snapshot: {e}"),
            BootError::Probe(e) => write!(f, "failed to read what the server holds: {e}"),
            BootError::Apply(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

fn spawn_options(socket: Option<&str>) -> SpawnOptions {
    SpawnOptions {
        socket: socket.map(str::to_string),
        ..Default::default()
    }
}

/// Everything on the server, read over a short-lived connection; `None`
/// where no session exists. The answer describes a server nothing locks: a
/// session built right after it is taken is still there when a restore
/// plans, which is why the restore captures again over its own connection.
fn survey(options: &SpawnOptions) -> Result<Option<Snapshot>, BootError> {
    let mut connection = match Connection::open(options, Attach::Existing, drop) {
        Ok((connection, _)) => connection,
        Err(TmuxError::NoSessions) => return Ok(None),
        Err(e) => return Err(BootError::Connect(e)),
    };
    capture(&mut connection, &Previous::default(), &Shells::default())
        .map(Some)
        .map_err(BootError::Probe)
}

/// What the server on `socket` holds; `None` where no session exists.
pub fn probe(socket: Option<&str>) -> Result<Option<ServerState>, BootError> {
    Ok(survey(&spawn_options(socket))?
        .as_ref()
        .map(ServerState::of))
}

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
                .all(|name| refused.sessions().iter().any(|s| s.name() == name)),
        }
    }
}

/// The boot decision made for one server. A reconnect to that server keeps it
/// rather than booting again, and the run advances it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    /// So a later reconnect can tell this server from one restarted on the
    /// same socket.
    pub server: Origin,
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
/// never restored into by a second look at it. Returns `Ok(None)` when the server has no
/// sessions and nothing is saved: there is nothing to attach to and nothing
/// to put there, and starting a server the user never asked for is not the
/// daemon's call, so the caller waits and asks again (DESIGN.md §8: "if tmux
/// isn't running … the daemon idles").
///
/// `on_notification` becomes the returned client's notification sink.
/// `on_log` receives one line saying which branch was taken, naming the
/// user's sessions when those are why nothing was restored.
pub fn connect_and_boot(
    socket: Option<String>,
    store: &Store,
    last: Option<&Decided>,
    mut on_log: impl FnMut(&str),
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Option<Booted>, BootError> {
    let options = spawn_options(socket.as_deref());
    let held = survey(&options)?.map(|live| (live.origin, ServerState::of(&live)));
    let returning = held
        .as_ref()
        .and_then(|(server, _)| last.filter(|last| last.server == *server));

    let (client, decided) = match (held, returning, store.latest()) {
        (_, Some(last), _) => {
            on_log(
                "reconnected to the server this daemon was running on; keeping its boot decision, \
                 not restoring into it",
            );
            (attach(&options, on_notification)?, last.clone())
        }
        (Some((server, ServerState::Built(built))), None, _) => {
            on_log(&format!(
                "server has session(s) the user built ({}); not restoring into it, staying in save mode",
                joined(&built)
            ));
            let boot = Boot::Declined(built);
            (attach(&options, on_notification)?, Decided { server, boot })
        }
        (_, None, Ok((generation, snapshot))) => {
            let (server, restored) = restore(&options, generation, &snapshot)?;
            on_log(&format!(
                "restored the latest snapshot: {} step(s) applied",
                restored.steps().len()
            ));
            // What the plan left alone or could not restore is the other
            // half of what happened.
            restored
                .notes()
                .iter()
                .for_each(|note| on_log(&note.to_string()));
            let boot = Boot::Settled;
            (attach(&options, on_notification)?, Decided { server, boot })
        }
        (None, None, Err(StoreError::NoLatest)) => {
            on_log("no sessions and no saved snapshot; waiting for a tmux session");
            return Ok(None);
        }
        (Some((server, _)), None, Err(StoreError::NoLatest)) => {
            on_log(
                "server holds only bootstrap sessions and nothing is saved yet; staying in save mode",
            );
            let boot = Boot::Settled;
            (attach(&options, on_notification)?, Decided { server, boot })
        }
        (_, None, Err(e)) => return Err(BootError::Store(e)),
    };
    Ok(Some(Booted { client, decided }))
}

/// Adds what the server lacks of `snapshot` over a connection of its own,
/// making a session to attach to where the server has none, and says which
/// server incarnation that was and what was done to it. The run loop still drives a `Client`, so the
/// caller attaches one afterwards; `tmux-laws-a4x.9xh` keeps this one
/// connection for the run instead.
fn restore(
    options: &SpawnOptions,
    generation: GenerationId,
    snapshot: &Snapshot,
) -> Result<(Origin, Plan), BootError> {
    let attach = Attach::OrCreate {
        name: scratch_name(snapshot),
    };
    let (mut connection, opened) =
        Connection::open(options, attach, drop).map_err(BootError::Connect)?;
    let live = capture(&mut connection, &Previous::default(), &Shells::default())
        .map_err(BootError::Probe)?;
    let restore_plan = plan(generation, snapshot, Onto::opened(&opened, &live));
    apply(&mut connection, &restore_plan).map_err(BootError::Apply)?;
    Ok((live.origin, restore_plan))
}

fn joined(names: &NonEmpty<SessionName>) -> String {
    names
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Bare `attach-session` attaches to the server's most recently used
/// session, which exists: the caller just read it, or restored into it.
fn attach(
    options: &SpawnOptions,
    on_notification: impl FnMut(ServerMessage) + 'static,
) -> Result<Client<SpawnTransport>, BootError> {
    let transport =
        SpawnTransport::spawn(&["attach-session"], options).map_err(BootError::Spawn)?;
    Client::connect(transport, on_notification, |_, _| {}).map_err(BootError::Connect)
}
