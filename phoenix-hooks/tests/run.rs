//! Live: `phoenix_hooks::run` against a real, isolated tmux server.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use phoenix_hooks::{run, Hook};

fn unique(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("phx-hooks-{name}-{}-{nanos}", std::process::id())
}

/// A throwaway server whose one pane runs `sh -i`, torn down one session at a
/// time — never a server-wide kill.
struct Server {
    socket: String,
    session: String,
}

impl Server {
    fn new(name: &str) -> Self {
        let server = Self {
            socket: format!("/tmp/{}", unique(name)),
            session: unique(name),
        };
        server.tmux(&["new-session", "-d", "-s", &server.session, "sh -i"]);
        server
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
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

    fn configure(&self, hook: Hook, command: &str) {
        self.tmux(&["set-option", "-g", &hook.option(), command]);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", &self.socket, "kill-session", "-t", &self.session])
            .status();
        let _ = std::fs::remove_file(&self.socket);
    }
}

struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(unique(name)))
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.0).expect("the hook should have written its file")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn a_point_with_no_hook_configured_runs_nothing_and_succeeds() {
    let server = Server::new("unset");
    run(Some(&server.socket), Hook::PreSave).expect("an unset hook is not a failure");
}

#[test]
fn a_hook_runs_its_text_as_written_with_the_points_argument() {
    let server = Server::new("verbatim");
    let out = TempFile::new("verbatim-out");
    // `#S` and `##` would be format-expanded if the text reached `run-shell`
    // unescaped, and the argument holds a quote, a space and a `#`.
    server.configure(
        Hook::PostSave {
            saved: Path::new(""),
        },
        &format!("printf '%s|%s\\n' \"$1\" '#S ##' > '{}'", out.0.display()),
    );

    let saved = Path::new("/tmp/a b'c#d");
    run(Some(&server.socket), Hook::PostSave { saved }).expect("the hook should succeed");

    assert_eq!(out.read(), "/tmp/a b'c#d|#S ##\n");
}

#[test]
fn a_hook_that_exits_non_zero_fails_naming_the_hook_and_carrying_its_output() {
    let server = Server::new("failing");
    server.configure(Hook::PreRestore, "echo boom >&2; exit 7");

    let err = run(Some(&server.socket), Hook::PreRestore)
        .expect_err("a non-zero exit must be an error")
        .to_string();

    assert!(err.contains("@phoenix-hook-pre-restore"), "{err}");
    assert!(
        err.contains("boom"),
        "the hook's stderr is part of the error: {err}"
    );
    assert!(err.contains("returned 7"), "{err}");
}

#[test]
fn a_bare_tmux_inside_a_hook_reaches_the_server_the_hook_runs_for() {
    let server = Server::new("own-server");
    server.configure(
        Hook::PostRestore,
        "tmux set-option -g @phoenix-test-seen yes",
    );

    run(Some(&server.socket), Hook::PostRestore).expect("the hook should succeed");

    assert_eq!(
        server.tmux(&["show-options", "-gqv", "@phoenix-test-seen"]),
        "yes\n"
    );
}

#[test]
fn a_hook_that_cannot_be_read_fails_naming_it() {
    let socket = format!("/tmp/{}", unique("no-server"));

    let err = run(Some(&socket), Hook::PreSave)
        .expect_err("no server means the hook cannot be read")
        .to_string();

    assert!(err.contains("could not read"), "{err}");
    assert!(err.contains("@phoenix-hook-pre-save"), "{err}");
}
