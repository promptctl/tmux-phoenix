//! Shared test-only harness for spawning an isolated, throw-away tmux
//! server that live-integration tests can safely drive without touching the
//! developer's real tmux sessions, plus the command builder and connection
//! every test drives it through. Duplicated (not shared via a lib crate)
//! from `tmux-control/tests/support/mod.rs` — see that copy's doc comment
//! for why.

use tmux_control::{Client, CommandLine, SpawnOptions, SpawnTransport};

/// A command line for tests, whose arguments never hold a NUL.
pub fn line(name: &'static str, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandLine {
    CommandLine::new(name, args).expect("test arguments hold no NUL")
}

pub struct IsolatedTmux {
    pub socket: String,
    pub session: String,
}

impl IsolatedTmux {
    pub fn new(name: &str) -> Self {
        let socket = format!("/tmp/tmux-phoenix-test-{name}-{}", std::process::id());
        let session = format!("phoenix-test-{name}");
        let status = std::process::Command::new("tmux")
            .args(["-S", &socket, "new-session", "-d", "-s", &session])
            .status()
            .expect("failed to run tmux new-session");
        assert!(status.success(), "tmux new-session failed");
        Self { socket, session }
    }

    /// A control-mode client attached to this server — the same connection
    /// the real `phoenix restore` drives.
    pub fn connect(&self) -> Client<SpawnTransport> {
        let transport = SpawnTransport::spawn(
            &["attach-session", "-t", &self.session],
            &SpawnOptions {
                socket: Some(self.socket.clone()),
                ..Default::default()
            },
        )
        .expect("failed to spawn tmux -C");
        Client::connect(transport, drop, |_, _| {}).expect("handshake failed against real tmux")
    }
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
    }
}

/// A window at its per-session index. Ids are unique across a test binary,
/// so windows from different sessions can share a snapshot.
pub fn linked(
    index: u32,
    name: &str,
    layout: &str,
    panes: phoenix_core::NonEmpty<phoenix_core::Pane>,
    active: u32,
) -> (phoenix_core::WindowIndex, phoenix_core::Window) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT_WINDOW_ID: AtomicU32 = AtomicU32::new(0);
    let window = phoenix_core::Window::new(
        phoenix_core::WindowId(NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed)),
        phoenix_core::Made::NotByPhoenix,
        phoenix_core::WindowName::parse(name).unwrap(),
        phoenix_core::Layout::parse(layout).unwrap(),
        false,
        panes,
        phoenix_core::PaneIndex(active),
    )
    .unwrap();
    (phoenix_core::WindowIndex(index), window)
}

/// One session and the windows it links, ready to join a snapshot.
pub struct Tree {
    pub session: phoenix_core::Session,
    pub windows: Vec<phoenix_core::Window>,
}

pub fn tree(
    name: &str,
    windows: phoenix_core::NonEmpty<(phoenix_core::WindowIndex, phoenix_core::Window)>,
    active: u32,
) -> Tree {
    let links: Vec<phoenix_core::WinLink> = windows
        .iter()
        .map(|(index, window)| phoenix_core::WinLink {
            index: *index,
            window: window.id(),
        })
        .collect();
    let session = phoenix_core::Session::new(
        phoenix_core::SessionName::parse(name).unwrap(),
        None,
        phoenix_core::NonEmpty::from_vec(links).unwrap(),
        phoenix_core::WindowIndex(active),
        None,
    )
    .unwrap();
    Tree {
        session,
        windows: windows.into_iter().map(|(_, w)| w).collect(),
    }
}

pub fn snapshot(trees: Vec<Tree>) -> phoenix_core::Snapshot {
    let mut windows = Vec::new();
    let mut sessions = Vec::new();
    for tree in trees {
        windows.extend(tree.windows);
        sessions.push(tree.session);
    }
    phoenix_core::Snapshot::new(
        phoenix_core::Origin::BeforeOriginWasRecorded,
        phoenix_core::Touched::Never,
        phoenix_core::OffsetDateTime::from_unix_timestamp(1_700_000_000),
        phoenix_core::TmuxVersion { major: 3, minor: 5 },
        phoenix_core::NonEmpty::from_vec(windows).unwrap(),
        phoenix_core::NonEmpty::from_vec(sessions).unwrap(),
        vec![],
    )
    .unwrap()
}
