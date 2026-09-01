//! Best-effort recovery of each pane's *foreground* program argv (DESIGN.md
//! §5) via a single system-wide `ps` pass — `pane_current_command` (from
//! `list-panes`) only gives the command name, and `pane_pid` is the pane's
//! shell, not whatever foreground job is currently running in it.
//!
//! Verified live against a real tmux server + `ps`: a pane's foreground
//! process is a descendant of `pane_pid` whose BSD `ps` `STAT` field carries
//! a trailing `+` (foreground process group of its controlling terminal) —
//! true of the shell itself when idle (state e.g. `Ss+`), and of whatever
//! child currently holds the foreground when a job is running (`S+`). This
//! avoids matching by tty, which is unreliable: pty device names get
//! recycled across unrelated sessions.

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub stat: String,
    pub command: String,
}

/// `-ww` disables `ps`'s terminal-width output truncation (verified live —
/// without it, long command lines get cut off when running non-interactively).
fn ps_command() -> Command {
    let mut cmd = Command::new("ps");
    cmd.args(["-wwAo", "pid=,ppid=,stat=,command="]);
    cmd
}

/// Pop one whitespace-delimited token off the front of `*s`, collapsing any
/// run of whitespace before it (unlike `str::splitn` with a char predicate,
/// which treats each whitespace byte as its own delimiter and so yields
/// empty fields between them — `ps`'s columns are space-padded, not
/// single-space-separated).
fn take_field<'a>(s: &mut &'a str) -> Option<&'a str> {
    let trimmed = s.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    let (field, rest) = trimmed.split_at(end);
    *s = rest;
    Some(field)
}

/// One line looks like `  1234   1 Ss+  /bin/zsh -l` — whitespace-separated
/// up to the command, which itself may contain spaces and so consumes the
/// rest of the line.
fn parse_ps_line(line: &str) -> Option<ProcessRow> {
    let mut rest = line;
    let pid: u32 = take_field(&mut rest)?.parse().ok()?;
    let ppid: u32 = take_field(&mut rest)?.parse().ok()?;
    let stat = take_field(&mut rest)?.to_string();
    let command = rest.trim_start();
    if command.is_empty() {
        return None;
    }
    Some(ProcessRow {
        pid,
        ppid,
        stat,
        command: command.to_string(),
    })
}

fn parse_ps_table(output: &str) -> Vec<ProcessRow> {
    output.lines().filter_map(parse_ps_line).collect()
}

/// Walk from `pane_pid` down through whichever child currently carries the
/// foreground `+` flag, stopping when no such child exists. Returns
/// `pane_pid`'s own row when the pane is idle at its shell (still commonly
/// `+`, e.g. `Ss+`) or when `pane_pid` itself can't be found (process
/// already exited — best-effort, so this degrades rather than errors).
fn foreground_row(pane_pid: u32, table: &[ProcessRow]) -> Option<&ProcessRow> {
    let mut current = table.iter().find(|p| p.pid == pane_pid)?;
    loop {
        let foreground_child = table
            .iter()
            .find(|p| p.ppid == current.pid && p.stat.contains('+'));
        match foreground_child {
            Some(child) => current = child,
            None => return Some(current),
        }
    }
}

/// Splitting a `ps` command string on whitespace is a lossy approximation of
/// real argv — `ps` prints the arguments space-joined, so a quoted argument
/// that itself contained a space can't be told apart from two arguments.
/// Accepted here because this whole path is explicitly best-effort recovery
/// (DESIGN.md §5), not a guarantee: the kernel's argv is not reachable
/// portably, and every other source of the same information has the same
/// flattening.
fn split_command(command: &str) -> Vec<String> {
    command.split_whitespace().map(str::to_string).collect()
}

/// Run one system-wide `ps`, then resolve each of `pane_pids` to its
/// foreground program's approximate argv. A pane missing from the result map
/// (process exited between `list-panes` and this call, or the whole `ps`
/// invocation failed) means recovery failed for that pane — the caller
/// treats that the same as an empty `argv` (DESIGN.md §5's "explicit
/// unknown-args, never laundered into a shell").
pub fn recover_argv(pane_pids: &[u32]) -> HashMap<u32, Vec<String>> {
    let output = match ps_command().output() {
        Ok(out) if out.status.success() => out.stdout,
        _ => return HashMap::new(),
    };
    let Ok(text) = String::from_utf8(output) else {
        return HashMap::new();
    };
    let table = parse_ps_table(&text);
    pane_pids
        .iter()
        .filter_map(|&pid| {
            foreground_row(pid, &table).map(|row| (pid, split_command(&row.command)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<ProcessRow> {
        vec![
            ProcessRow {
                pid: 100,
                ppid: 1,
                stat: "Ss".to_string(),
                command: "/bin/zsh -l".to_string(),
            },
            ProcessRow {
                pid: 200,
                ppid: 1,
                stat: "Ss+".to_string(),
                command: "/bin/zsh -l".to_string(),
            },
            ProcessRow {
                pid: 201,
                ppid: 200,
                stat: "S+".to_string(),
                command: "vim DESIGN.md".to_string(),
            },
        ]
    }

    #[test]
    fn parse_ps_line_splits_leading_columns_and_keeps_command_intact() {
        let row = parse_ps_line("  1234   1 Ss+  /bin/zsh -l --login").unwrap();
        assert_eq!(row.pid, 1234);
        assert_eq!(row.ppid, 1);
        assert_eq!(row.stat, "Ss+");
        assert_eq!(row.command, "/bin/zsh -l --login");
    }

    #[test]
    fn parse_ps_table_skips_unparseable_lines() {
        let rows = parse_ps_table("  1   0 Ss  /sbin/launchd\nnot a row\n");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn foreground_row_returns_the_idle_shell_when_no_foreground_child() {
        let table = table();
        let row = foreground_row(100, &table).unwrap();
        assert_eq!(row.pid, 100);
    }

    #[test]
    fn foreground_row_descends_to_the_running_foreground_child() {
        let table = table();
        let row = foreground_row(200, &table).unwrap();
        assert_eq!(row.pid, 201);
        assert_eq!(row.command, "vim DESIGN.md");
    }

    #[test]
    fn foreground_row_is_none_when_pane_pid_is_not_in_the_table() {
        let table = table();
        assert!(foreground_row(9999, &table).is_none());
    }

    #[test]
    fn split_command_is_a_whitespace_split() {
        assert_eq!(
            split_command("vim DESIGN.md"),
            vec!["vim".to_string(), "DESIGN.md".to_string()]
        );
    }
}
