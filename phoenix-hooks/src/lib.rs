//! `phoenix-hooks` — runs the shell command a user configured for one point in
//! a save or restore (tmux-parity-ure.9).
//!
//! A hook is a tmux global user option holding shell text, as tmux-resurrect's
//! `@resurrect-hook-*` options are:
//!
//! ```text
//! set -g @phoenix-hook-post-save 'cp "$1" ~/snapshots/'
//! ```
//!
//! tmux is the one home of that configuration (`[LAW:one-source-of-truth]`):
//! the CLI and a daemon started by launchd read the same `.tmux.conf`, and each
//! point reads its option when it is reached, so a re-sourced config applies
//! from the next point on.
//!
//! The command runs through the server's own `run-shell`, so it sees what a
//! command run from tmux sees: `/bin/sh`, the server's global environment
//! (`$DISPLAY`, `$WINDOWID`), and `TMUX` naming that server, so a bare `tmux`
//! inside a hook reaches the server being saved or restored even when it is
//! not the default one. Verified live against tmux 3.6a: `run-shell` waits for
//! the command, exits with its status, relays its stdout, drops its stderr, and
//! expands `#` formats in its argument. So the text goes over with stderr
//! folded into stdout and every `#` doubled, and a hook runs exactly as
//! written, the way resurrect's `eval` runs it.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::process::{Command, Output};

use tmux_control::socket_args;

const OPTION_PREFIX: &str = "@phoenix-hook-";

/// A point in a save or restore where a configured command runs, carrying what
/// that point hands the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook<'a> {
    /// Before a save captures anything. A failure aborts the save.
    PreSave,
    /// After a save is written; `$1` is the saved generation's path.
    PostSave { saved: &'a Path },
    /// Before a restore's plan applies. A failure aborts the restore.
    PreRestore,
    /// After a restore has put every session in place.
    PostRestore,
}

impl Hook<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            Hook::PreSave => "pre-save",
            Hook::PostSave { .. } => "post-save",
            Hook::PreRestore => "pre-restore",
            Hook::PostRestore => "post-restore",
        }
    }

    /// The tmux option holding this point's command.
    pub fn option(&self) -> String {
        format!("{OPTION_PREFIX}{}", self.name())
    }

    fn args(&self) -> Vec<&[u8]> {
        match self {
            Hook::PostSave { saved } => vec![saved.as_os_str().as_bytes()],
            Hook::PreSave | Hook::PreRestore | Hook::PostRestore => Vec::new(),
        }
    }
}

/// A hook that could not be read, or whose command failed. Names the hook, and
/// carries tmux's output, which holds the command's own.
#[derive(Debug)]
pub struct HookError {
    hook: &'static str,
    failure: Failure,
}

#[derive(Debug)]
enum Failure {
    Read(String),
    Run(String),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hook = self.hook;
        match &self.failure {
            Failure::Read(detail) => write!(
                f,
                "could not read the {hook} hook ({OPTION_PREFIX}{hook}): {detail}"
            ),
            Failure::Run(detail) => {
                write!(
                    f,
                    "the {hook} hook ({OPTION_PREFIX}{hook}) failed: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for HookError {}

/// Runs the command configured for `hook` on the server at `socket` and waits
/// for it. An unset option is a point with nothing configured.
pub fn run(socket: Option<&str>, hook: Hook) -> Result<(), HookError> {
    let fail = |failure| HookError {
        hook: hook.name(),
        failure,
    };
    let configured = tmux(
        socket,
        ["show-options".into(), "-gqv".into(), hook.option().into()],
    )
    .map_err(|detail| fail(Failure::Read(detail)))?;
    match configured_command(&configured) {
        None => Ok(()),
        Some(command) => tmux(socket, ["run-shell".into(), script(command, &hook.args())])
            .map(drop)
            .map_err(|detail| fail(Failure::Run(detail))),
    }
}

/// The command in `show-options -v`'s output, which ends the value with one
/// newline of its own; an unset option prints nothing.
fn configured_command(shown: &[u8]) -> Option<&[u8]> {
    Some(shown.strip_suffix(b"\n").unwrap_or(shown)).filter(|command| !command.is_empty())
}

/// What `run-shell` is handed: stderr folded into the stdout tmux relays,
/// `args` as the positional parameters, then `command`, with every `#` doubled
/// so tmux's format expansion gives `/bin/sh` back exactly this text.
fn script(command: &[u8], args: &[&[u8]]) -> OsString {
    let mut text = b"exec 2>&1\nset --".to_vec();
    for arg in args {
        text.push(b' ');
        text.extend(single_quoted(arg));
    }
    text.push(b'\n');
    text.extend_from_slice(command);
    OsString::from_vec(
        text.into_iter()
            .flat_map(|byte| match byte {
                b'#' => vec![b'#', b'#'],
                other => vec![other],
            })
            .collect(),
    )
}

/// `arg` as one `/bin/sh` word: inside single quotes nothing is special except
/// the closing quote, so each `'` closes, adds an escaped quote, and reopens.
fn single_quoted(arg: &[u8]) -> Vec<u8> {
    let mut word = vec![b'\''];
    for &byte in arg {
        match byte {
            b'\'' => word.extend_from_slice(b"'\\''"),
            other => word.push(other),
        }
    }
    word.push(b'\'');
    word
}

/// Runs one plain `tmux` command and returns its stdout. Failing to start it,
/// or a non-zero exit, is an error carrying what it printed, or its exit status
/// when it printed nothing (`[LAW:no-silent-failure]`).
fn tmux<const N: usize>(socket: Option<&str>, args: [OsString; N]) -> Result<Vec<u8>, String> {
    let out = Command::new("tmux")
        .args(socket_args(socket))
        .args(args)
        .output()
        .map_err(|e| format!("failed to run tmux: {e}"))?;
    match out.status.success() {
        true => Ok(out.stdout),
        false => Err(failure_detail(&out)),
    }
}

fn failure_detail(out: &Output) -> String {
    let printed = [out.stdout.as_slice(), out.stderr.as_slice()].concat();
    let printed = String::from_utf8_lossy(&printed).trim().to_string();
    match printed.is_empty() {
        true => format!("tmux exited with {}", out.status),
        false => printed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_options_output_yields_the_value_without_its_trailing_newline() {
        assert_eq!(configured_command(b"echo hi\n"), Some(&b"echo hi"[..]));
        assert_eq!(configured_command(b"a\nb\n"), Some(&b"a\nb"[..]));
    }

    #[test]
    fn an_unset_option_configures_nothing() {
        assert_eq!(configured_command(b""), None);
    }

    #[test]
    fn a_hook_is_named_by_its_option() {
        assert_eq!(Hook::PreSave.option(), "@phoenix-hook-pre-save");
        assert_eq!(
            Hook::PostSave {
                saved: Path::new("/x")
            }
            .option(),
            "@phoenix-hook-post-save"
        );
        assert_eq!(Hook::PreRestore.option(), "@phoenix-hook-pre-restore");
        assert_eq!(Hook::PostRestore.option(), "@phoenix-hook-post-restore");
    }
}
