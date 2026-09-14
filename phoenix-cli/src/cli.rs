//! Pure argument parsing (DESIGN.md §9) — no I/O, testable without a tmux
//! server. Hand-rolled: the surface is two subcommands and two flags.

pub const DEFAULT_KEEP_GENERATIONS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Save { keep: usize, socket: Option<String> },
    List,
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
        Some("--help") | Some("-h") | None => Ok(Command::Help),
        Some(other) => Err(format!(
            "unknown subcommand {other:?} (try \"save\" or \"list\")"
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
}
