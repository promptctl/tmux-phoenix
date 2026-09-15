//! Pure argument parsing (DESIGN.md §9) — no I/O, testable without a tmux
//! server. Hand-rolled: the surface is five subcommands and a few flags.

use std::num::NonZeroUsize;

// [LAW:parse-dont-validate] retention is non-zero from the flag down: a
// store that keeps zero generations would delete the save it just wrote.
pub const DEFAULT_KEEP_GENERATIONS: NonZeroUsize = NonZeroUsize::new(10).unwrap();
pub const DEFAULT_DEBOUNCE_SECS: u64 = 10;
pub const DEFAULT_MAX_INTERVAL_SECS: u64 = 300;

/// What `phoenix daemon` runs with. `install` takes the same flags and
/// writes them into the service it generates, so the two can't disagree on
/// what a flag means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonSettings {
    pub keep: NonZeroUsize,
    pub debounce_secs: u64,
    pub max_interval_secs: u64,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            keep: DEFAULT_KEEP_GENERATIONS,
            debounce_secs: DEFAULT_DEBOUNCE_SECS,
            max_interval_secs: DEFAULT_MAX_INTERVAL_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Save {
        keep: NonZeroUsize,
        socket: Option<String>,
    },
    List,
    Restore {
        dry_run: bool,
        file: Option<String>,
        socket: Option<String>,
    },
    Daemon {
        settings: DaemonSettings,
        socket: Option<String>,
    },
    /// No socket: the installed service runs `phoenix daemon` against the
    /// default server, the one a login session uses.
    Install {
        settings: DaemonSettings,
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
        Some("install") => parse_install(&args[1..]),
        Some("--help") | Some("-h") | None => Ok(Command::Help),
        Some(other) => Err(format!(
            "unknown subcommand {other:?} (try \"save\", \"list\", \"restore\", \"daemon\" or \"install\")"
        )),
    }
}

fn parse_save(args: &[String]) -> Result<Command, String> {
    let mut keep = DEFAULT_KEEP_GENERATIONS;
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--keep" => keep = flag_value(args, &mut i)?,
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

/// The value after the flag at `args[*i]`, parsed; advances `*i` onto it.
fn flag_value<T>(args: &[String], i: &mut usize) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let flag = &args[*i];
    *i += 1;
    let value = args.get(*i).ok_or(format!("{flag} requires a value"))?;
    value
        .parse()
        .map_err(|e| format!("{flag}: {value:?} is not accepted ({e})"))
}

/// Consumes `args[*i]` into `settings` when it is one of the
/// [`DaemonSettings`] flags; `Ok(false)` leaves any other argument to the
/// caller.
fn take_setting(
    settings: &mut DaemonSettings,
    args: &[String],
    i: &mut usize,
) -> Result<bool, String> {
    match args[*i].as_str() {
        "--keep" => settings.keep = flag_value(args, i)?,
        "--debounce" => settings.debounce_secs = flag_value(args, i)?,
        "--max-interval" => settings.max_interval_secs = flag_value(args, i)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_daemon(args: &[String]) -> Result<Command, String> {
    let mut settings = DaemonSettings::default();
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        if !take_setting(&mut settings, args, &mut i)? {
            match args[i].as_str() {
                "--socket" => {
                    i += 1;
                    socket = Some(args.get(i).ok_or("--socket requires a value")?.clone());
                }
                other => return Err(format!("daemon: unknown argument {other:?}")),
            }
        }
        i += 1;
    }
    Ok(Command::Daemon { settings, socket })
}

fn parse_install(args: &[String]) -> Result<Command, String> {
    let mut settings = DaemonSettings::default();
    let mut i = 0;
    while i < args.len() {
        if !take_setting(&mut settings, args, &mut i)? {
            return Err(format!("install: unknown argument {:?}", args[i]));
        }
        i += 1;
    }
    Ok(Command::Install { settings })
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
                keep: NonZeroUsize::new(3).unwrap(),
                socket: Some("/tmp/s".to_string())
            }
        );
    }

    #[test]
    fn save_rejects_a_non_numeric_keep() {
        assert!(parse_args(&args(&["save", "--keep", "abc"])).is_err());
    }

    #[test]
    fn every_keep_flag_rejects_zero_where_it_is_parsed() {
        for subcommand in ["save", "daemon", "install"] {
            let err = parse_args(&args(&[subcommand, "--keep", "0"])).unwrap_err();
            assert!(err.contains("--keep"), "{subcommand}: {err}");
        }
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
                settings: DaemonSettings::default(),
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
                settings: DaemonSettings {
                    keep: NonZeroUsize::new(3).unwrap(),
                    debounce_secs: 5,
                    max_interval_secs: 120,
                },
                socket: Some("/tmp/s".to_string()),
            }
        );
    }

    #[test]
    fn daemon_rejects_unknown_flags_dangling_values_and_non_numbers() {
        assert!(parse_args(&args(&["daemon", "--nope"])).is_err());
        assert!(parse_args(&args(&["daemon", "--debounce"])).is_err());
        assert!(parse_args(&args(&["daemon", "--max-interval", "soon"])).is_err());
    }

    #[test]
    fn install_takes_the_daemon_settings() {
        assert_eq!(
            parse_args(&args(&["install", "--keep", "3", "--debounce", "5"])).unwrap(),
            Command::Install {
                settings: DaemonSettings {
                    keep: NonZeroUsize::new(3).unwrap(),
                    debounce_secs: 5,
                    max_interval_secs: DEFAULT_MAX_INTERVAL_SECS,
                },
            }
        );
    }

    #[test]
    fn install_rejects_a_socket_flag_and_unknown_flags() {
        assert!(parse_args(&args(&["install", "--socket", "/tmp/s"])).is_err());
        assert!(parse_args(&args(&["install", "--nope"])).is_err());
    }
}
