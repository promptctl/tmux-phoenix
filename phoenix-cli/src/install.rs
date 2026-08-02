//! `phoenix install` (DESIGN.md §8/§9): writes a `launchd` user agent
//! (macOS) or `systemd --user` unit (Linux) that runs `phoenix daemon`.
//! Splits into a pure "what would we write, and where" (testable without
//! touching the filesystem) and a thin effectful wrapper that actually
//! writes it — same pattern as everywhere else in this codebase.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub file_path: PathBuf,
    pub contents: String,
    /// The command the user runs themselves to actually activate the
    /// service — `phoenix install` writes the file but never enables it
    /// automatically (starting a persistent, reboot-surviving background
    /// process is the user's call to make, not a side effect of writing a
    /// config file).
    pub enable_hint: String,
}

fn daemon_args(keep: usize, debounce_secs: u64, max_interval_secs: u64) -> String {
    format!("daemon --keep {keep} --debounce {debounce_secs} --max-interval {max_interval_secs}")
}

pub fn launchd_plan(
    exe: &Path,
    home: &Path,
    keep: usize,
    debounce_secs: u64,
    max_interval_secs: u64,
) -> InstallPlan {
    const LABEL: &str = "com.tmux-phoenix.daemon";
    let file_path = home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    let log_dir = home.join("Library/Logs");
    let stdout_log = log_dir.join("tmux-phoenix.log");
    let stderr_log = log_dir.join("tmux-phoenix.err.log");

    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>daemon</string>
		<string>--keep</string>
		<string>{keep}</string>
		<string>--debounce</string>
		<string>{debounce_secs}</string>
		<string>--max-interval</string>
		<string>{max_interval_secs}</string>
	</array>
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
        exe = exe.display(),
        stdout = stdout_log.display(),
        stderr = stderr_log.display(),
    );

    InstallPlan {
        file_path: file_path.clone(),
        contents,
        enable_hint: format!("launchctl load -w {}", file_path.display()),
    }
}

pub fn systemd_plan(
    exe: &Path,
    home: &Path,
    keep: usize,
    debounce_secs: u64,
    max_interval_secs: u64,
) -> InstallPlan {
    let file_path = home
        .join(".config/systemd/user")
        .join("tmux-phoenix.service");

    let contents = format!(
        "[Unit]\n\
         Description=tmux-phoenix daemon\n\
         \n\
         [Service]\n\
         ExecStart={exe} {args}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        args = daemon_args(keep, debounce_secs, max_interval_secs),
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
    keep: usize,
    debounce_secs: u64,
    max_interval_secs: u64,
) -> Option<InstallPlan> {
    if cfg!(target_os = "macos") {
        Some(launchd_plan(
            exe,
            home,
            keep,
            debounce_secs,
            max_interval_secs,
        ))
    } else if cfg!(target_os = "linux") {
        Some(systemd_plan(
            exe,
            home,
            keep,
            debounce_secs,
            max_interval_secs,
        ))
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

    #[test]
    fn launchd_plan_embeds_the_exe_path_and_daemon_flags() {
        let plan = launchd_plan(
            Path::new("/usr/local/bin/phoenix"),
            Path::new("/Users/test"),
            7,
            15,
            600,
        );
        assert_eq!(
            plan.file_path,
            Path::new("/Users/test/Library/LaunchAgents/com.tmux-phoenix.daemon.plist")
        );
        assert!(plan.contents.contains("/usr/local/bin/phoenix"));
        assert!(plan.contents.contains("<string>7</string>"));
        assert!(plan.contents.contains("<string>15</string>"));
        assert!(plan.contents.contains("<string>600</string>"));
        assert!(
            plan.contents.contains("<true/>"),
            "RunAtLoad/KeepAlive should be set"
        );
        assert!(plan.enable_hint.contains("launchctl load"));
    }

    #[test]
    fn systemd_plan_embeds_the_exe_path_and_daemon_flags() {
        let plan = systemd_plan(
            Path::new("/usr/local/bin/phoenix"),
            Path::new("/home/test"),
            7,
            15,
            600,
        );
        assert_eq!(
            plan.file_path,
            Path::new("/home/test/.config/systemd/user/tmux-phoenix.service")
        );
        assert!(plan.contents.contains(
            "ExecStart=/usr/local/bin/phoenix daemon --keep 7 --debounce 15 --max-interval 600"
        ));
        assert!(plan.contents.contains("WantedBy=default.target"));
        assert!(plan.enable_hint.contains("systemctl --user enable --now"));
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
