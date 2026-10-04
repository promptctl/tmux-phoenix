//! A throwaway tmux server on its own socket for live tests, never the
//! developer's own, with the snapshot builders the tests share.

use std::time::{SystemTime, UNIX_EPOCH};

use phoenix_capture::Previous;
use phoenix_core::{
    Content, ContentFailure, Cwd, Foreground, GenerationId, Layout, Made, NonEmpty, OffsetDateTime,
    Origin, Pane, PaneId, PaneIndex, Session, SessionName, Shells, Snapshot, TmuxVersion, Touched,
    WinLink, Window, WindowId, WindowIndex, WindowName,
};
use phoenix_restore::{apply, plan, scratch_name, Onto, Plan};
use tmux_control::{Attach, Connection, SpawnOptions};

/// A shell with no startup files is idle at its prompt and stays there; the
/// developer's own runs programs of its own at times no test controls.
const IDLE_SHELL: &str = "bash --norc --noprofile";

pub const GEN: GenerationId = GenerationId(7);

pub struct Server {
    pub socket: String,
}

impl Server {
    /// A socket no server runs on yet.
    pub fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self {
            socket: format!("/tmp/phx-restore-{name}-{}-{nanos}", std::process::id()),
        }
    }

    /// A server holding one session of one idle shell, whose later panes are
    /// idle shells too.
    pub fn holding(name: &str, session: &str) -> Self {
        let server = Self::new(name);
        server.tmux(&[
            "new-session",
            "-d",
            "-s",
            session,
            "-x",
            "100",
            "-y",
            "30",
            IDLE_SHELL,
            ";",
            "set-option",
            "-g",
            "default-command",
            IDLE_SHELL,
        ]);
        server
    }

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("tmux")
            .args(["-S", &self.socket])
            .args(args)
            .output()
            .expect("failed to run tmux");
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn lines(&self, args: &[&str]) -> Vec<String> {
        self.tmux(args).lines().map(str::to_string).collect()
    }

    pub fn session_names(&self) -> Vec<String> {
        let mut names = self.lines(&["list-sessions", "-F", "#{session_name}"]);
        names.sort();
        names
    }

    /// What `phoenix restore` does: open a connection, making a session to
    /// attach to where the server has none; capture; plan; apply.
    pub fn restore(&self, snapshot: &Snapshot) -> Plan {
        let options = SpawnOptions {
            socket: Some(self.socket.clone()),
            ..Default::default()
        };
        let attach = Attach::OrCreate {
            name: scratch_name(snapshot),
        };
        let (mut connection, opened) =
            Connection::open(&options, attach, drop).expect("failed to open a connection");
        let live =
            phoenix_capture::capture(&mut connection, &Previous::default(), &Shells::default())
                .expect("failed to capture the live server");
        let plan = plan(GEN, snapshot, Onto::opened(&opened, &live));
        apply(&mut connection, &plan).expect("apply failed");
        plan
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // One session at a time — never a server-wide kill.
        let listing = std::process::Command::new("tmux")
            .args(["-S", &self.socket, "list-sessions", "-F", "#{session_name}"])
            .output();
        if let Ok(out) = listing {
            for name in String::from_utf8_lossy(&out.stdout).lines() {
                let _ = std::process::Command::new("tmux")
                    .args([
                        "-S",
                        &self.socket,
                        "kill-session",
                        "-t",
                        &format!("={name}:"),
                    ])
                    .status();
            }
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

pub fn pane(index: u32, cwd: &str) -> Pane {
    Pane {
        id: PaneId(index),
        index: PaneIndex(index),
        cwd: Cwd::parse(cwd),
        foreground: Foreground::Shell,
        content: Content::NotCaptured {
            reason: ContentFailure::NotRecorded,
        },
    }
}

/// One idle pane of 80x24 at `/tmp`.
pub fn window(id: u32, name: &str) -> Window {
    panes(id, name, "b25d,80x24,0,0,0", vec![pane(0, "/tmp")], 0)
}

pub fn panes(id: u32, name: &str, layout: &str, panes: Vec<Pane>, active: u32) -> Window {
    Window::new(
        WindowId(id),
        Made::NotByPhoenix,
        WindowName::parse(name).unwrap(),
        Layout::parse(layout).unwrap(),
        false,
        NonEmpty::from_vec(panes).unwrap(),
        PaneIndex(active),
    )
    .unwrap()
}

/// `links` are `(index, window id)`; the first is the active one.
pub fn session(name: &str, links: &[(u32, u32)]) -> Session {
    let links: Vec<WinLink> = links
        .iter()
        .map(|(index, window)| WinLink {
            index: WindowIndex(*index),
            window: WindowId(*window),
        })
        .collect();
    let active = links[0].index;
    Session::new(
        SessionName::parse(name).unwrap(),
        None,
        NonEmpty::from_vec(links).unwrap(),
        active,
        None,
    )
    .unwrap()
}

pub fn snapshot(windows: Vec<Window>, sessions: Vec<Session>) -> Snapshot {
    Snapshot::new(
        Origin::BeforeOriginWasRecorded,
        Touched::Never,
        OffsetDateTime::from_unix_timestamp(1_700_000_000),
        TmuxVersion { major: 3, minor: 7 },
        NonEmpty::from_vec(windows).unwrap(),
        NonEmpty::from_vec(sessions).unwrap(),
        vec![],
    )
    .unwrap()
}

/// Polls `read` until `done` accepts what it returns, and returns that; a
/// pane's shell prints, and tmux resolves a new pane's cwd, shortly after
/// the command that caused it returns.
pub fn eventually<T>(mut read: impl FnMut() -> T, done: impl Fn(&T) -> bool) -> T {
    for _ in 0..50 {
        let value = read();
        if done(&value) {
            return value;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    read()
}
