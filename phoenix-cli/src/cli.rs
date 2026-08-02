//! Pure argument parsing (DESIGN.md §9) — no I/O, testable without a tmux
//! server. No CLI-parsing crate is available in this environment (see
//! DESIGN.md §4's implementation note), so this is hand-rolled; the surface
//! is small enough (two subcommands, two flags) that it doesn't need one.

pub const DEFAULT_KEEP_GENERATIONS: usize = 10;
pub const DEFAULT_DEBOUNCE_SECS: u64 = 10;
pub const DEFAULT_MAX_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Save {
        keep: usize,
        socket: Option<String>,
    },
    List,
    Restore {
        dry_run: bool,
        file: Option<String>,
        socket: Option<String>,
    },
    Daemon {
        keep: usize,
        debounce_secs: u64,
        max_interval_secs: u64,
        socket: Option<String>,
    },
    Help,
}

pub fn parse_args(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("save") => parse_save(&args[1..]),
        Some("list") => {
            if args.len() > 1 {
                return Err(format!("list: unknown argument {:?}", args[1]));
            }
            Ok(Command::List)
        }
        Some("restore") => parse_restore(&args[1..]),
        Some("daemon") => parse_daemon(&args[1..]),
        Some("--help") | Some("-h") | None => Ok(Command::Help),
        Some(other) => Err(format!(
            "unknown subcommand {other:?} (try \"save\", \"list\", \"restore\", or \"daemon\")"
        )),
    }
}

fn parse_save(args: &[String]) -> Result<Command, String> {
    let mut keep = DEFAULT_KEEP_GENERATIONS;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--keep" => {
                i += 1;
                let value = args.get(i).ok_or("--keep requires a value")?;
                keep = value
                    .parse()
                    .map_err(|_| format!("--keep: {value:?} is not a non-negative integer"))?;
            }
            "--socket" => {
                i += 1;
                socket = Some(args.get(i).ok_or("--socket requires a value")?.clone());
            }
            other => return Err(format!("save: unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(Command::Save { keep, socket })
}

fn parse_restore(args: &[String]) -> Result<Command, String> {
    let mut dry_run = false;
    let mut file = None;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => dry_run = true,
            "--file" => {
                i += 1;
                file = Some(args.get(i).ok_or("--file requires a value")?.clone());
            }
            "--socket" => {
                i += 1;
                socket = Some(args.get(i).ok_or("--socket requires a value")?.clone());
            }
            other => return Err(format!("restore: unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(Command::Restore {
        dry_run,
        file,
        socket,
    })
}

fn parse_daemon(args: &[String]) -> Result<Command, String> {
    let mut keep = DEFAULT_KEEP_GENERATIONS;
    let mut debounce_secs = DEFAULT_DEBOUNCE_SECS;
    let mut max_interval_secs = DEFAULT_MAX_INTERVAL_SECS;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--keep" => {
                i += 1;
                let value = args.get(i).ok_or("--keep requires a value")?;
                keep = value
                    .parse()
                    .map_err(|_| format!("--keep: {value:?} is not a non-negative integer"))?;
            }
            "--debounce" => {
                i += 1;
                let value = args.get(i).ok_or("--debounce requires a value")?;
                debounce_secs = value
                    .parse()
                    .map_err(|_| format!("--debounce: {value:?} is not a non-negative integer"))?;
            }
            "--max-interval" => {
                i += 1;
                let value = args.get(i).ok_or("--max-interval requires a value")?;
                max_interval_secs = value.parse().map_err(|_| {
                    format!("--max-interval: {value:?} is not a non-negative integer")
                })?;
            }
            "--socket" => {
                i += 1;
                socket = Some(args.get(i).ok_or("--socket requires a value")?.clone());
            }
            other => return Err(format!("daemon: unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(Command::Daemon {
        keep,
        debounce_secs,
        max_interval_secs,
        socket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_help() {
        assert_eq!(parse_args(&[]).unwrap(), Command::Help);
    }

    #[test]
    fn help_flags_are_help() {
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse_args(&args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        assert!(parse_args(&args(&["frobnicate"])).is_err());
    }

    #[test]
    fn bare_save_uses_defaults() {
        assert_eq!(
            parse_args(&args(&["save"])).unwrap(),
            Command::Save {
                keep: DEFAULT_KEEP_GENERATIONS,
                socket: None
            }
        );
    }

    #[test]
    fn save_with_keep_and_socket() {
        assert_eq!(
            parse_args(&args(&["save", "--keep", "3", "--socket", "/tmp/s"])).unwrap(),
            Command::Save {
                keep: 3,
                socket: Some("/tmp/s".to_string())
            }
        );
    }

    #[test]
    fn save_rejects_a_non_numeric_keep() {
        assert!(parse_args(&args(&["save", "--keep", "abc"])).is_err());
    }

    #[test]
    fn save_rejects_a_dangling_flag() {
        assert!(parse_args(&args(&["save", "--keep"])).is_err());
        assert!(parse_args(&args(&["save", "--socket"])).is_err());
    }

    #[test]
    fn save_rejects_unknown_flags() {
        assert!(parse_args(&args(&["save", "--nope"])).is_err());
    }

    #[test]
    fn bare_list_is_ok() {
        assert_eq!(parse_args(&args(&["list"])).unwrap(), Command::List);
    }

    #[test]
    fn list_rejects_extra_arguments() {
        assert!(parse_args(&args(&["list", "--socket", "/tmp/s"])).is_err());
    }

    #[test]
    fn bare_restore_uses_defaults() {
        assert_eq!(
            parse_args(&args(&["restore"])).unwrap(),
            Command::Restore {
                dry_run: false,
                file: None,
                socket: None,
            }
        );
    }

    #[test]
    fn restore_with_all_flags() {
        assert_eq!(
            parse_args(&args(&[
                "restore",
                "--dry-run",
                "--file",
                "/tmp/snap",
                "--socket",
                "/tmp/s"
            ]))
            .unwrap(),
            Command::Restore {
                dry_run: true,
                file: Some("/tmp/snap".to_string()),
                socket: Some("/tmp/s".to_string()),
            }
        );
    }

    #[test]
    fn restore_rejects_a_dangling_file_flag() {
        assert!(parse_args(&args(&["restore", "--file"])).is_err());
    }

    #[test]
    fn restore_rejects_unknown_flags() {
        assert!(parse_args(&args(&["restore", "--nope"])).is_err());
    }

    #[test]
    fn bare_daemon_uses_defaults() {
        assert_eq!(
            parse_args(&args(&["daemon"])).unwrap(),
            Command::Daemon {
                keep: DEFAULT_KEEP_GENERATIONS,
                debounce_secs: DEFAULT_DEBOUNCE_SECS,
                max_interval_secs: DEFAULT_MAX_INTERVAL_SECS,
                socket: None,
            }
        );
    }

    #[test]
    fn daemon_with_all_flags() {
        assert_eq!(
            parse_args(&args(&[
                "daemon",
                "--keep",
                "3",
                "--debounce",
                "5",
                "--max-interval",
                "120",
                "--socket",
                "/tmp/s"
            ]))
            .unwrap(),
            Command::Daemon {
                keep: 3,
                debounce_secs: 5,
                max_interval_secs: 120,
                socket: Some("/tmp/s".to_string()),
            }
        );
    }

    #[test]
    fn daemon_rejects_unknown_flags() {
        assert!(parse_args(&args(&["daemon", "--nope"])).is_err());
    }
}
