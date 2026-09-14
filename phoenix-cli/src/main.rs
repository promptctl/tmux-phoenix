mod cli;
mod commands;

use cli::Command;

const HELP: &str = "\
phoenix — save, inspect and restore tmux sessions

USAGE:
    phoenix save [--keep N] [--socket PATH]
    phoenix list
    phoenix restore [--dry-run] [--file PATH] [--socket PATH]
    phoenix --help

save: exit 0 on a clean save, 3 if some pane's foreground program or working
directory couldn't be fully recovered (the snapshot is still saved), 1 on
failure. Prints the saved generation's path on stdout.
list: one saved generation per line on stdout, tab-separated
(captured_at_unix, format_version, path), newest first.
restore: rebuilds the latest (or --file) snapshot into new tmux sessions,
each pane back at its captured working directory and running the program it
was running. A pane that was idle at its shell comes back as an idle shell.
--dry-run prints the exact tmux commands that would run without running them.";

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
    };

    std::process::exit(code);
}
