mod cli;
mod commands;
mod install;

use cli::Command;

const HELP: &str = "\
phoenix — save, inspect, restore and continuously checkpoint tmux sessions

USAGE:
    phoenix save [--keep N] [--socket PATH]
    phoenix list
    phoenix restore [--dry-run] [--file PATH] [--socket PATH]
    phoenix daemon [--keep N] [--debounce SECS] [--max-interval SECS] [--socket PATH]
    phoenix install [--keep N] [--debounce SECS] [--max-interval SECS]
    phoenix --help

save: exit 0 on a clean save, 3 if some pane's foreground program or working
directory couldn't be fully recovered or the post-save hook failed (the
snapshot is still saved), 1 on failure, including a failed pre-save hook and a
server holding only untouched bootstrap sessions (one window, one idle shell
each), which is never saved. Prints the saved generation's path on stdout.
--keep N (save, daemon, install): how many generations to keep, at least 1
(default 10); older ones are pruned after each save.
list: one saved generation per line on stdout, tab-separated
(captured_at_unix, format_version, path), newest first.
restore: rebuilds the latest (or --file) snapshot into new tmux sessions,
replacing the server's sessions only when every one is a bootstrap session,
each pane back at its captured working directory and running the program it
was running. A pane that was idle at its shell comes back as an idle shell.
--dry-run prints the tmux commands that would run without running them. A pane's
scrollback replay prints as a `#` summary line: its command names a temp file
that only exists once restore actually runs. Exit 0 when restored, 3 if the
post-restore hook failed (the restore stands), 1 on failure, including a failed
pre-restore hook.
daemon: runs in the foreground until killed. Saves pane structure and
scrollback once activity has been quiet for --debounce seconds (default 10),
with a --max-interval backstop (default 300) so idle sessions still
checkpoint. On a server with no sessions, or only bootstrap sessions (what a
terminal starting tmux creates), it restores the latest snapshot in their
place; on one holding a session you built it never restores. Keeps retrying
while tmux is not running.
install: writes a launchd user agent (macOS) or systemd --user unit (Linux)
that runs \"phoenix daemon\" with these settings, and prints the command to
activate it. Never activates the service itself.
hooks: a shell command set as a tmux global option runs, through the server's
run-shell, at that point of every save and restore, the daemon's included:
@phoenix-hook-pre-save, @phoenix-hook-post-save (given the saved path as $1),
@phoenix-hook-pre-restore and @phoenix-hook-post-restore. A failing pre-hook
stops the save or restore. A finished restore sets @phoenix-restored to the
restored snapshot's capture time, for tmux-side integrations to wait on.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse_args(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("phoenix: {msg}");
            eprintln!("{HELP}");
            std::process::exit(commands::EXIT_FAIL);
        }
    };

    let code = match command {
        Command::Help => {
            println!("{HELP}");
            commands::EXIT_OK
        }
        Command::Save { keep, socket } => commands::run_save(keep, socket),
        Command::List => commands::run_list(),
        Command::Restore {
            dry_run,
            file,
            socket,
        } => commands::run_restore(dry_run, file, socket),
        Command::Daemon { settings, socket } => commands::run_daemon(settings, socket),
        Command::Install { settings } => commands::run_install(settings),
    };

    std::process::exit(code);
}
