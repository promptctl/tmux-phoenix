mod cli;
mod commands;
mod install;

use cli::Command;

const HELP: &str = "\
phoenix — save, inspect, restore, and continuously checkpoint tmux sessions

USAGE:
    phoenix save [--keep N] [--socket PATH]
    phoenix list
    phoenix restore [--dry-run] [--file PATH] [--socket PATH]
    phoenix daemon [--keep N] [--debounce SECS] [--max-interval SECS] [--socket PATH]
    phoenix install [--keep N] [--debounce SECS] [--max-interval SECS]
    phoenix --help

save: exit 0 on a clean save, 3 if some pane's foreground program couldn't
be fully recovered (the snapshot is still saved), 1 on failure.
list: one saved generation per line on stdout, tab-separated
(captured_at_unix, format_version, path), newest first.
restore: rebuilds the latest (or --file) snapshot into a new tmux session,
shell + cwd only by default — never blind-replays a captured program. May
prompt once per pane whether to actually relaunch its captured program
(once / always-exact / always-like / no); grants are remembered in
${XDG_CONFIG_HOME}/tmux-phoenix/relaunch.rules. --dry-run never prompts and
prints the exact tmux commands that would run without running them.
daemon: runs foreground (for supervision, wrap this in a launchd/systemd
unit). Saves once activity has been quiet for --debounce seconds, with a
--max-interval backstop so long-idle sessions still checkpoint.
install: writes a launchd user agent (macOS) or systemd --user unit (Linux)
that runs \"phoenix daemon\", and prints the command to activate it. Never
activates the service itself.";

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
        Command::Daemon {
            keep,
            debounce_secs,
            max_interval_secs,
            socket,
        } => commands::run_daemon(keep, debounce_secs, max_interval_secs, socket),
        Command::Install {
            keep,
            debounce_secs,
            max_interval_secs,
        } => commands::run_install(keep, debounce_secs, max_interval_secs),
    };

    std::process::exit(code);
}
