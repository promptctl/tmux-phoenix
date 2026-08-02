//! tmux-permissions-16s: "A matcher (Exact command | Program basename) +
//! verdict (Restore|Skip), resolved by a pure function to Restore/Skip/Ask.
//! Interactive: an Ask prompts once / always-exact / always-like / no
//! (interpreters default to Exact so a wide grant is deliberate).
//! Non-interactive (boot): an Ask falls to the safe default (cwd+shell) and
//! is logged — never prompt-blocks, never silently escalates. Rules persist
//! in one human-editable file." See DESIGN.md §6.
//!
//! Everything here except [`load_rules_file`]/[`save_rules_file`] is pure —
//! actual terminal interaction (reading what the user typed) is the
//! caller's job (`phoenix-cli`, for `phoenix restore`); this module only
//! decides what a given typed choice *means*, so that decision is testable
//! without a real TTY. [`crate::plan::plan`] never sees any of this
//! directly — it only reads the [`crate::policy::RestorePolicy`] that
//! [`resolve_interactive`]/[`resolve_non_interactive`] build ahead of time.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use phoenix_core::{CapturedProgram, Snapshot};

use crate::policy::{PaneLocation, RestorePolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Matches a pane's captured argv, rejoined with single spaces (the
    /// same join used both when a rule is learned and when a live pane is
    /// checked against it — an opaque comparison key, never re-split back
    /// into individual arguments).
    Exact(String),
    /// Matches `CapturedProgram::command` — tmux's own
    /// `pane_current_command`, already a bare program name with no path or
    /// arguments.
    Basename(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Restore,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub matcher: Matcher,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Restore,
    Skip,
    Ask,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
}

/// The join used everywhere argv needs to become a single comparison key —
/// see [`Matcher::Exact`]'s doc comment for why this never needs to be
/// un-joined.
fn cmdline(argv: &[String]) -> String {
    argv.join(" ")
}

impl RuleSet {
    /// A pane with empty `argv` means best-effort recovery failed for it
    /// (`CapturedProgram`'s doc comment) — there is no known command to ask
    /// about, so this always resolves to `Skip`, in both the interactive
    /// and non-interactive paths alike, before any rule is even consulted.
    /// Otherwise: an `Exact` rule wins over a `Basename` one (more specific
    /// intent should never be shadowed by a broader one learned earlier or
    /// later), first match in each category wins.
    pub fn resolve(&self, program: &CapturedProgram) -> Resolution {
        if program.argv.is_empty() {
            return Resolution::Skip;
        }
        let cmd = cmdline(&program.argv);
        let exact = self.rules.iter().find_map(|r| match &r.matcher {
            Matcher::Exact(value) if *value == cmd => Some(r.verdict),
            _ => None,
        });
        let basename = self.rules.iter().find_map(|r| match &r.matcher {
            Matcher::Basename(value) if *value == program.command => Some(r.verdict),
            _ => None,
        });
        match exact.or(basename) {
            Some(Verdict::Restore) => Resolution::Restore,
            Some(Verdict::Skip) => Resolution::Skip,
            None => Resolution::Ask,
        }
    }

    /// One rule per non-comment, non-blank line: `<restore|skip>
    /// <exact|basename> <value>` — `value` is everything after the second
    /// space, unsplit, so an `Exact` value containing spaces round-trips
    /// exactly. Malformed lines are collected as warnings rather than
    /// failing the whole file — a hand-editable file with one bad line
    /// shouldn't lose every other rule in it (`[LAW:no-silent-failure]`:
    /// still reported, just not fatal).
    pub fn parse(text: &str) -> (RuleSet, Vec<String>) {
        let mut rules = Vec::new();
        let mut warnings = Vec::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_rule_line(line) {
                Some(rule) => rules.push(rule),
                None => warnings.push(format!("line {}: malformed rule: {raw:?}", lineno + 1)),
            }
        }
        (RuleSet { rules }, warnings)
    }

    pub fn render(&self) -> String {
        let mut out = String::from(
            "# tmux-phoenix relaunch rules — consent grants for program relaunch on restore\n\
             # <restore|skip> <exact|basename> <value>\n",
        );
        for rule in &self.rules {
            let verdict = match rule.verdict {
                Verdict::Restore => "restore",
                Verdict::Skip => "skip",
            };
            match &rule.matcher {
                Matcher::Exact(value) => out.push_str(&format!("{verdict} exact {value}\n")),
                Matcher::Basename(value) => out.push_str(&format!("{verdict} basename {value}\n")),
            }
        }
        out
    }
}

fn parse_rule_line(line: &str) -> Option<Rule> {
    let mut parts = line.splitn(3, ' ');
    let verdict = match parts.next()? {
        "restore" => Verdict::Restore,
        "skip" => Verdict::Skip,
        _ => return None,
    };
    let kind = parts.next()?;
    let value = parts.next()?.to_string();
    if value.is_empty() {
        return None;
    }
    let matcher = match kind {
        "exact" => Matcher::Exact(value),
        "basename" => Matcher::Basename(value),
        _ => return None,
    };
    Some(Rule { matcher, verdict })
}

/// Programs whose basename alone is too broad a thing to auto-grant by
/// default: "restore-like any python invocation" is a much wider grant than
/// the user asking about one specific script likely intends. The prompt
/// still *offers* `always-like`; this only decides which choice a bare
/// Enter picks — see [`default_choice_for`].
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "python", "python2", "python3", "ruby", "irb",
    "pry", "perl", "node", "nodejs", "deno", "bun", "php", "lua", "R", "Rscript", "julia", "ghci",
    "psql", "sqlite3",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptChoice {
    Once,
    AlwaysExact,
    AlwaysLike,
    No,
}

/// The choice a bare Enter should pick for `command` — `AlwaysExact` for a
/// known interpreter (see [`INTERPRETERS`]), `AlwaysLike` otherwise, where a
/// basename grant is a reasonably-scoped default.
pub fn default_choice_for(command: &str) -> PromptChoice {
    if INTERPRETERS.contains(&command) {
        PromptChoice::AlwaysExact
    } else {
        PromptChoice::AlwaysLike
    }
}

/// Recognizes the four documented responses (full word or single-letter);
/// `None` for anything else, so the caller's interactive loop can re-prompt
/// rather than silently guessing what the user meant.
pub fn parse_prompt_choice(input: &str) -> Option<PromptChoice> {
    match input.trim().to_ascii_lowercase().as_str() {
        "o" | "once" => Some(PromptChoice::Once),
        "e" | "exact" | "always-exact" => Some(PromptChoice::AlwaysExact),
        "l" | "like" | "always-like" => Some(PromptChoice::AlwaysLike),
        "n" | "no" => Some(PromptChoice::No),
        _ => None,
    }
}

/// Applies an already-made choice: `Once` resolves this pane only and
/// changes nothing persisted; `AlwaysExact`/`AlwaysLike` additionally learn
/// a new `Restore` rule into `ruleset` so future restores of the same (or
/// same-basename) program stop asking. Never returns `Ask` — by
/// construction, a choice was already made.
pub fn apply_choice(
    choice: PromptChoice,
    program: &CapturedProgram,
    ruleset: &mut RuleSet,
) -> Resolution {
    match choice {
        PromptChoice::Once => Resolution::Restore,
        PromptChoice::AlwaysExact => {
            ruleset.rules.push(Rule {
                matcher: Matcher::Exact(cmdline(&program.argv)),
                verdict: Verdict::Restore,
            });
            Resolution::Restore
        }
        PromptChoice::AlwaysLike => {
            ruleset.rules.push(Rule {
                matcher: Matcher::Basename(program.command.clone()),
                verdict: Verdict::Restore,
            });
            Resolution::Restore
        }
        PromptChoice::No => Resolution::Skip,
    }
}

fn pane_locations_with_programs(snapshot: &Snapshot) -> Vec<(PaneLocation, CapturedProgram)> {
    let mut out = Vec::new();
    for session in snapshot.sessions.iter() {
        for window in session.windows().iter() {
            for pane in window.panes().iter() {
                out.push((
                    PaneLocation {
                        session: session.name().clone(),
                        window: window.index(),
                        pane: pane.index,
                    },
                    pane.program.clone(),
                ));
            }
        }
    }
    out
}

fn relaunch_entry(
    location: PaneLocation,
    program: &CapturedProgram,
) -> (PaneLocation, Vec<String>) {
    (location, program.argv.clone())
}

/// Walks every pane in `snapshot`, consulting `ruleset`; an `Ask` calls
/// `ask` (real stdin/stdout belongs to the caller — this stays testable
/// with a mock closure) to get a [`PromptChoice`], applying it via
/// [`apply_choice`] (which may grow `ruleset` with a new learned rule).
/// Returns the resulting policy plus whether `ruleset` changed, so the
/// caller knows whether it's worth persisting.
pub fn resolve_interactive(
    snapshot: &Snapshot,
    ruleset: &mut RuleSet,
    mut ask: impl FnMut(&PaneLocation, &CapturedProgram) -> PromptChoice,
) -> RestorePolicy {
    let mut relaunch = HashMap::new();
    for (location, program) in pane_locations_with_programs(snapshot) {
        let resolution = match ruleset.resolve(&program) {
            Resolution::Ask => apply_choice(ask(&location, &program), &program, ruleset),
            other => other,
        };
        if resolution == Resolution::Restore {
            let (location, argv) = relaunch_entry(location, &program);
            relaunch.insert(location, argv);
        }
    }
    RestorePolicy { relaunch }
}

/// The non-interactive (boot restore) counterpart: an `Ask` never prompts —
/// it falls straight to the safe default (`Skip`, i.e. cwd + shell only)
/// and is reported through `on_ask` so the caller can log it, per DESIGN.md
/// §6/§8's "never prompt-blocks, never silently escalates." `ruleset` is
/// read-only here — boot restore never learns new rules on its own.
pub fn resolve_non_interactive(
    snapshot: &Snapshot,
    ruleset: &RuleSet,
    mut on_ask: impl FnMut(&PaneLocation, &CapturedProgram),
) -> RestorePolicy {
    let mut relaunch = HashMap::new();
    for (location, program) in pane_locations_with_programs(snapshot) {
        let resolution = match ruleset.resolve(&program) {
            Resolution::Ask => {
                on_ask(&location, &program);
                Resolution::Skip
            }
            other => other,
        };
        if resolution == Resolution::Restore {
            let (location, argv) = relaunch_entry(location, &program);
            relaunch.insert(location, argv);
        }
    }
    RestorePolicy { relaunch }
}

/// `${XDG_CONFIG_HOME}/tmux-phoenix/relaunch.rules`, falling back to
/// `~/.config/tmux-phoenix/relaunch.rules` — config, not data
/// (`phoenix_store::Store::xdg_default`'s directory), since this file is
/// meant to be hand-edited, not machine-generated.
pub fn default_rules_path() -> io::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg)
                .join("tmux-phoenix")
                .join("relaunch.rules"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/tmux-phoenix/relaunch.rules"))
}

/// An absent file is a normal, expected state (nothing learned yet) — not
/// an error: returns an empty [`RuleSet`], same as an empty file would.
pub fn load_rules_file(path: &Path) -> io::Result<(RuleSet, Vec<String>)> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(RuleSet::parse(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((RuleSet::default(), Vec::new())),
        Err(e) => Err(e),
    }
}

/// Same temp+rename atomicity `phoenix-store` uses for its own files — a
/// crash mid-write must never leave a half-written rules file that silently
/// drops previously-learned grants.
pub fn save_rules_file(path: &Path, ruleset: &RuleSet) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("rules.tmp");
    fs::write(&tmp, ruleset.render())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(command: &str, argv: &[&str]) -> CapturedProgram {
        CapturedProgram::new(command, argv.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn no_matching_rule_is_ask() {
        let rs = RuleSet::default();
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "a.rs"])),
            Resolution::Ask
        );
    }

    #[test]
    fn empty_argv_is_always_skip_even_with_a_matching_basename_rule() {
        let rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Basename("zsh".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        assert_eq!(rs.resolve(&program("zsh", &[])), Resolution::Skip);
    }

    #[test]
    fn a_basename_rule_matches_any_argv_with_that_command() {
        let rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Basename("vim".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "a.rs"])),
            Resolution::Restore
        );
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "b.rs"])),
            Resolution::Restore
        );
    }

    #[test]
    fn an_exact_rule_only_matches_that_exact_argv() {
        let rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Exact("vim a.rs".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "a.rs"])),
            Resolution::Restore
        );
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "b.rs"])),
            Resolution::Ask
        );
    }

    #[test]
    fn exact_wins_over_a_conflicting_basename_rule() {
        let rs = RuleSet {
            rules: vec![
                Rule {
                    matcher: Matcher::Basename("vim".to_string()),
                    verdict: Verdict::Skip,
                },
                Rule {
                    matcher: Matcher::Exact("vim a.rs".to_string()),
                    verdict: Verdict::Restore,
                },
            ],
        };
        assert_eq!(
            rs.resolve(&program("vim", &["vim", "a.rs"])),
            Resolution::Restore
        );
    }

    #[test]
    fn parse_round_trips_through_render() {
        let rs = RuleSet {
            rules: vec![
                Rule {
                    matcher: Matcher::Basename("vim".to_string()),
                    verdict: Verdict::Restore,
                },
                Rule {
                    matcher: Matcher::Exact("git log --oneline".to_string()),
                    verdict: Verdict::Skip,
                },
            ],
        };
        let (parsed, warnings) = RuleSet::parse(&rs.render());
        assert!(warnings.is_empty());
        assert_eq!(parsed, rs);
    }

    #[test]
    fn parse_ignores_blank_lines_and_comments() {
        let (rs, warnings) = RuleSet::parse("\n# a comment\n\nrestore basename vim\n");
        assert!(warnings.is_empty());
        assert_eq!(rs.rules.len(), 1);
    }

    #[test]
    fn parse_reports_malformed_lines_as_warnings_without_dropping_the_rest() {
        let (rs, warnings) = RuleSet::parse("restore basename vim\nnonsense line\n");
        assert_eq!(rs.rules.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonsense line"));
    }

    #[test]
    fn interpreters_default_to_exact_others_default_to_like() {
        assert_eq!(default_choice_for("python3"), PromptChoice::AlwaysExact);
        assert_eq!(default_choice_for("bash"), PromptChoice::AlwaysExact);
        assert_eq!(default_choice_for("vim"), PromptChoice::AlwaysLike);
        assert_eq!(default_choice_for("htop"), PromptChoice::AlwaysLike);
    }

    #[test]
    fn parse_prompt_choice_recognizes_words_and_letters_case_insensitively() {
        assert_eq!(parse_prompt_choice("Once"), Some(PromptChoice::Once));
        assert_eq!(parse_prompt_choice("e"), Some(PromptChoice::AlwaysExact));
        assert_eq!(parse_prompt_choice("LIKE"), Some(PromptChoice::AlwaysLike));
        assert_eq!(parse_prompt_choice("n"), Some(PromptChoice::No));
        assert_eq!(parse_prompt_choice("huh"), None);
    }

    #[test]
    fn apply_choice_once_restores_without_learning_a_rule() {
        let mut rs = RuleSet::default();
        let resolution = apply_choice(
            PromptChoice::Once,
            &program("vim", &["vim", "a.rs"]),
            &mut rs,
        );
        assert_eq!(resolution, Resolution::Restore);
        assert!(rs.rules.is_empty());
    }

    #[test]
    fn apply_choice_always_exact_learns_an_exact_rule() {
        let mut rs = RuleSet::default();
        apply_choice(
            PromptChoice::AlwaysExact,
            &program("vim", &["vim", "a.rs"]),
            &mut rs,
        );
        assert_eq!(
            rs.rules,
            vec![Rule {
                matcher: Matcher::Exact("vim a.rs".to_string()),
                verdict: Verdict::Restore,
            }]
        );
    }

    #[test]
    fn apply_choice_always_like_learns_a_basename_rule() {
        let mut rs = RuleSet::default();
        apply_choice(
            PromptChoice::AlwaysLike,
            &program("vim", &["vim", "a.rs"]),
            &mut rs,
        );
        assert_eq!(
            rs.rules,
            vec![Rule {
                matcher: Matcher::Basename("vim".to_string()),
                verdict: Verdict::Restore,
            }]
        );
    }

    #[test]
    fn apply_choice_no_skips_without_learning_anything() {
        let mut rs = RuleSet::default();
        let resolution = apply_choice(PromptChoice::No, &program("vim", &["vim", "a.rs"]), &mut rs);
        assert_eq!(resolution, Resolution::Skip);
        assert!(rs.rules.is_empty());
    }

    use phoenix_core::{
        FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneIndex, Session, SessionName,
        TmuxVersion, Window, WindowIndex, WindowName,
    };

    fn pane_with_program(index: u32, program: CapturedProgram) -> Pane {
        Pane {
            index: PaneIndex(index),
            cwd: "/home/user".into(),
            program,
            content: None,
        }
    }

    fn snapshot_with_one_pane(program: CapturedProgram) -> Snapshot {
        let win = Window::new(
            WindowIndex(0),
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            NonEmpty::singleton(pane_with_program(0, program)),
            PaneIndex(0),
        )
        .unwrap();
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(win),
            WindowIndex(0),
        )
        .unwrap();
        Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions: NonEmpty::singleton(session),
        }
    }

    #[test]
    fn resolve_interactive_never_asks_when_a_rule_already_decides() {
        let mut rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Basename("vim".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        let snapshot = snapshot_with_one_pane(program("vim", &["vim", "a.rs"]));
        let mut asked = false;
        let policy = resolve_interactive(&snapshot, &mut rs, |_, _| {
            asked = true;
            PromptChoice::No
        });
        assert!(!asked);
        assert_eq!(policy.relaunch.len(), 1);
    }

    #[test]
    fn resolve_interactive_asks_and_learns_on_ask() {
        let mut rs = RuleSet::default();
        let snapshot = snapshot_with_one_pane(program("vim", &["vim", "a.rs"]));
        let policy = resolve_interactive(&snapshot, &mut rs, |_, _| PromptChoice::AlwaysLike);
        assert_eq!(policy.relaunch.len(), 1);
        assert_eq!(rs.rules.len(), 1, "AlwaysLike should have learned a rule");
    }

    #[test]
    fn resolve_interactive_a_no_answer_skips_and_learns_nothing() {
        let mut rs = RuleSet::default();
        let snapshot = snapshot_with_one_pane(program("vim", &["vim", "a.rs"]));
        let policy = resolve_interactive(&snapshot, &mut rs, |_, _| PromptChoice::No);
        assert!(policy.relaunch.is_empty());
        assert!(rs.rules.is_empty());
    }

    #[test]
    fn resolve_non_interactive_never_asks_it_defaults_to_skip_and_reports() {
        let rs = RuleSet::default();
        let snapshot = snapshot_with_one_pane(program("vim", &["vim", "a.rs"]));
        let mut reported = 0;
        let policy = resolve_non_interactive(&snapshot, &rs, |_, _| reported += 1);
        assert!(policy.relaunch.is_empty());
        assert_eq!(reported, 1);
    }

    #[test]
    fn resolve_non_interactive_still_honors_an_already_learned_rule() {
        let rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Basename("vim".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        let snapshot = snapshot_with_one_pane(program("vim", &["vim", "a.rs"]));
        let mut reported = 0;
        let policy = resolve_non_interactive(&snapshot, &rs, |_, _| reported += 1);
        assert_eq!(
            reported, 0,
            "an already-decided pane must never be reported as an ask"
        );
        assert_eq!(policy.relaunch.len(), 1);
    }

    #[test]
    fn load_rules_file_on_a_missing_path_is_an_empty_ruleset_not_an_error() {
        let path = std::env::temp_dir().join(format!(
            "phoenix-permission-test-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (rs, warnings) = load_rules_file(&path).unwrap();
        assert!(rs.rules.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "phoenix-permission-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("relaunch.rules");
        let rs = RuleSet {
            rules: vec![Rule {
                matcher: Matcher::Basename("vim".to_string()),
                verdict: Verdict::Restore,
            }],
        };
        save_rules_file(&path, &rs).unwrap();
        let (loaded, warnings) = load_rules_file(&path).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(loaded, rs);
        let _ = fs::remove_dir_all(&dir);
    }
}
