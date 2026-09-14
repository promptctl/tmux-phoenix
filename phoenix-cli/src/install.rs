//! `phoenix install` (DESIGN.md §8/§9): writes a `launchd` user agent
//! (macOS) or `systemd --user` unit (Linux) that runs `phoenix daemon`.
//! Split into a pure "what would we write, and where" (testable without
//! touching the filesystem) and a thin effectful wrapper that writes it.

use std::path::{Path, PathBuf};

use crate::cli::DaemonSettings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub file_path: PathBuf,
    pub contents: String,
    /// The shell command the user runs to activate the service —
    /// `phoenix install` writes the file but never enables it.
    pub enable_hint: String,
}

const LAUNCHD_LABEL: &str = "com.tmux-phoenix.daemon";

/// The `phoenix daemon` argv the service runs, after the executable.
fn daemon_args(settings: DaemonSettings) -> Vec<String> {
    vec![
        "daemon".to_string(),
        "--keep".to_string(),
        settings.keep.to_string(),
        "--debounce".to_string(),
        settings.debounce_secs.to_string(),
        "--max-interval".to_string(),
        settings.max_interval_secs.to_string(),
    ]
}

/// Text content for a plist `<string>`: a path holding `&` or `<` would
/// otherwise produce a file launchd refuses to parse.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// One `ExecStart=` word. systemd splits the line on whitespace, expands `%`
/// specifiers and `$` variables, and honours C-style escapes inside double
/// quotes, so each of those has to be neutralised for a path to arrive
/// verbatim.
fn systemd_word(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// POSIX-shell single-quoting for the printed enable hint.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn launchd_plan(exe: &Path, home: &Path, settings: DaemonSettings) -> InstallPlan {
    let file_path = home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    let log_dir = home.join("Library/Logs");

    let program_arguments: String = std::iter::once(exe.display().to_string())
        .chain(daemon_args(settings))
        .map(|arg| format!("\t\t<string>{}</string>\n", xml_escape(&arg)))
        .collect();

    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCHD_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{program_arguments}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{stdout}</string>
	<key>StandardErrorPath</key>
	<string>{stderr}</string>
</dict>
</plist>
"#,
        stdout = xml_escape(&log_dir.join("tmux-phoenix.log").display().to_string()),
        stderr = xml_escape(&log_dir.join("tmux-phoenix.err.log").display().to_string()),
    );

    let enable_hint = format!(
        "launchctl bootstrap gui/$(id -u) {}",
        shell_quote(&file_path.display().to_string())
    );
    InstallPlan {
        file_path,
        contents,
        enable_hint,
    }
}

pub fn systemd_plan(exe: &Path, home: &Path, settings: DaemonSettings) -> InstallPlan {
    let file_path = home
        .join(".config/systemd/user")
        .join("tmux-phoenix.service");

    let exec_start = std::iter::once(exe.display().to_string())
        .chain(daemon_args(settings))
        .map(|arg| systemd_word(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    let contents = format!(
        "[Unit]\n\
         Description=tmux-phoenix daemon\n\
         \n\
         [Service]\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );

    InstallPlan {
        file_path,
        contents,
        enable_hint: "systemctl --user enable --now tmux-phoenix.service".to_string(),
    }
}

/// `None` on a platform neither `launchd_plan` nor `systemd_plan` covers.
pub fn plan_for_this_platform(
    exe: &Path,
    home: &Path,
    settings: DaemonSettings,
) -> Option<InstallPlan> {
    if cfg!(target_os = "macos") {
        Some(launchd_plan(exe, home, settings))
    } else if cfg!(target_os = "linux") {
        Some(systemd_plan(exe, home, settings))
    } else {
        None
    }
}

pub fn write(plan: &InstallPlan) -> std::io::Result<()> {
    if let Some(parent) = plan.file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plan.file_path, &plan.contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> DaemonSettings {
        DaemonSettings {
            keep: 7,
            debounce_secs: 15,
            max_interval_secs: 600,
        }
    }

    #[test]
    fn launchd_plan_embeds_the_exe_path_and_daemon_flags() {
        let plan = launchd_plan(
            Path::new("/usr/local/bin/phoenix"),
            Path::new("/Users/test"),
            settings(),
        );
        assert_eq!(
            plan.file_path,
            Path::new("/Users/test/Library/LaunchAgents/com.tmux-phoenix.daemon.plist")
        );
        assert!(plan.contents.contains(
            "\t\t<string>/usr/local/bin/phoenix</string>\n\t\t<string>daemon</string>\n"
        ));
        assert!(plan.contents.contains("<string>7</string>"));
        assert!(plan.contents.contains("<string>15</string>"));
        assert!(plan.contents.contains("<string>600</string>"));
        assert!(plan.enable_hint.starts_with("launchctl bootstrap gui/"));
    }

    #[test]
    fn launchd_plan_escapes_xml_in_paths() {
        let plan = launchd_plan(
            Path::new("/opt/R&D <tools>/phoenix"),
            Path::new("/Users/a&b"),
            settings(),
        );
        assert!(plan
            .contents
            .contains("<string>/opt/R&amp;D &lt;tools&gt;/phoenix</string>"));
        assert!(plan
            .contents
            .contains("<string>/Users/a&amp;b/Library/Logs/tmux-phoenix.log</string>"));
        assert!(!plan.contents.contains("R&D"));
    }

    #[test]
    fn systemd_plan_embeds_the_exe_path_and_daemon_flags() {
        let plan = systemd_plan(
            Path::new("/usr/local/bin/phoenix"),
            Path::new("/home/test"),
            settings(),
        );
        assert_eq!(
            plan.file_path,
            Path::new("/home/test/.config/systemd/user/tmux-phoenix.service")
        );
        assert!(plan.contents.contains(
            "ExecStart=\"/usr/local/bin/phoenix\" \"daemon\" \"--keep\" \"7\" \"--debounce\" \"15\" \"--max-interval\" \"600\"\n"
        ));
        assert!(plan.contents.contains("WantedBy=default.target"));
        assert!(plan.enable_hint.contains("systemctl --user enable --now"));
    }

    #[test]
    fn systemd_words_survive_spaces_quotes_specifiers_and_variables() {
        assert_eq!(
            systemd_word(r#"/home/me/my "bin"/50%$HOME\x"#),
            r#""/home/me/my \"bin\"/50%%$$HOME\\x""#
        );
    }

    #[test]
    fn write_creates_parent_directories_and_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "phoenix-install-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let plan = InstallPlan {
            file_path: dir.join("nested/tmux-phoenix.service"),
            contents: "hello".to_string(),
            enable_hint: String::new(),
        };
        write(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(&plan.file_path).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
