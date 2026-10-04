//! The two OS reads that decide a pane's foreground (ARCHITECTURE.md §5),
//! keyed by the pane's pid and nothing else: which process group holds the
//! pane's terminal, then that group leader's exact argv. No `ps`, no
//! whitespace splitting, no walk of the process tree — the kernel holds
//! both facts and hands them over per pid.
//!
//! Every failure is a per-pane [`Foreground::Unrecovered`] with its reason
//! (`[LAW:no-silent-failure]`); nothing here returns an empty map.

use phoenix_core::{Foreground, RecoveryFailure, Shells, TerminalHolder};

/// The process group facts about one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalGroup {
    /// The process's own group.
    pub pgid: i32,
    /// The foreground group of its controlling terminal; `0` or negative
    /// when it has none.
    pub tpgid: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProcessError {
    /// No such process any more.
    Gone,
    Os(String),
}

#[cfg(target_os = "macos")]
mod os {
    //! `sysctl KERN_PROC_PID` for the group facts and `KERN_PROCARGS2` for
    //! argv. `kinfo_proc` is 648 bytes on 64-bit Darwin with `kp_proc.p_pid`
    //! at 40, `kp_eproc.e_pgid` at 564 and `kp_eproc.e_tpgid` at 576 —
    //! verified with `offsetof` on Darwin 25 (2026-10-04). A reply of any
    //! other size is refused rather than read at guessed offsets.

    use super::{ProcessError, TerminalGroup};
    use phoenix_core::NonEmpty;
    use std::ffi::c_void;
    use std::io;

    extern "C" {
        fn sysctl(
            name: *const i32,
            namelen: u32,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> i32;
    }

    const CTL_KERN: i32 = 1;
    const KERN_PROC: i32 = 14;
    const KERN_PROC_PID: i32 = 1;
    const KERN_PROCARGS2: i32 = 49;

    const KINFO_PROC_LEN: usize = 648;
    const P_PID: usize = 40;
    const E_PGID: usize = 564;
    const E_TPGID: usize = 576;

    fn read_i32(buf: &[u8], at: usize) -> i32 {
        i32::from_le_bytes(buf[at..at + 4].try_into().expect("four bytes"))
    }

    fn os_error() -> ProcessError {
        ProcessError::Os(io::Error::last_os_error().to_string())
    }

    pub(super) fn terminal_group(pid: u32) -> Result<TerminalGroup, ProcessError> {
        let mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid as i32];
        let mut buf = [0u8; KINFO_PROC_LEN];
        let mut len = KINFO_PROC_LEN;
        // SAFETY: `mib` has four elements, `buf` has `len` bytes, and the
        // kernel writes at most `len` bytes back into it.
        let rc = unsafe {
            sysctl(
                mib.as_ptr(),
                4,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(os_error());
        }
        // A pid that no longer exists is answered with success and zero bytes
        // (verified live), so emptiness is the "gone" signal here.
        if len == 0 {
            return Err(ProcessError::Gone);
        }
        if len != KINFO_PROC_LEN {
            return Err(ProcessError::Os(format!(
                "kinfo_proc is {len} bytes, expected {KINFO_PROC_LEN}"
            )));
        }
        if read_i32(&buf, P_PID) != pid as i32 {
            return Err(ProcessError::Gone);
        }
        Ok(TerminalGroup {
            pgid: read_i32(&buf, E_PGID),
            tpgid: read_i32(&buf, E_TPGID),
        })
    }

    /// `KERN_PROCARGS2` lays the reply out as: `argc` (a native `i32`), the
    /// executable path, NUL padding, then `argc` NUL-terminated argument
    /// strings, then the environment (verified live, 2026-10-04).
    pub(super) fn argv(pid: u32) -> Result<NonEmpty<String>, ProcessError> {
        let mib = [CTL_KERN, KERN_PROCARGS2, pid as i32];
        let mut len = 0usize;
        // SAFETY: a null `oldp` with a valid `oldlenp` asks only for the size.
        let rc = unsafe {
            sysctl(
                mib.as_ptr(),
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(gone_or_os());
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` holds `len` bytes, the size the kernel just asked for.
        let rc = unsafe {
            sysctl(
                mib.as_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(gone_or_os());
        }
        buf.truncate(len);
        parse_procargs2(&buf)
    }

    /// `KERN_PROCARGS2` answers a vanished pid with `EINVAL` (verified live);
    /// `ESRCH` is the documented spelling. Both mean the leader is gone.
    fn gone_or_os() -> ProcessError {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(3) | Some(22) => ProcessError::Gone,
            _ => ProcessError::Os(err.to_string()),
        }
    }

    fn parse_procargs2(buf: &[u8]) -> Result<NonEmpty<String>, ProcessError> {
        let malformed = || ProcessError::Os("KERN_PROCARGS2 reply was malformed".to_string());
        let argc = usize::try_from(read_i32(buf.get(..4).ok_or_else(malformed)?, 0))
            .map_err(|_| malformed())?;
        let rest = &buf[4..];
        let path_end = rest.iter().position(|&b| b == 0).ok_or_else(malformed)?;
        let mut rest = &rest[path_end..];
        while let Some((&0, tail)) = rest.split_first() {
            rest = tail;
        }
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            let end = rest.iter().position(|&b| b == 0).ok_or_else(malformed)?;
            args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            rest = &rest[end + 1..];
        }
        NonEmpty::from_vec(args)
            .ok_or_else(|| ProcessError::Os("process reported an empty argv".to_string()))
    }
}

#[cfg(target_os = "linux")]
mod os {
    //! `/proc/<pid>/stat` for the group facts (`pgrp` and `tpgid` are the
    //! third and sixth fields after the parenthesised `comm`, per proc(5))
    //! and `/proc/<pid>/cmdline` for argv, NUL-separated.

    use super::{ProcessError, TerminalGroup};
    use phoenix_core::NonEmpty;
    use std::io;

    fn read(path: &str) -> Result<Vec<u8>, ProcessError> {
        std::fs::read(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ProcessError::Gone,
            _ => ProcessError::Os(e.to_string()),
        })
    }

    pub(super) fn terminal_group(pid: u32) -> Result<TerminalGroup, ProcessError> {
        let stat = read(&format!("/proc/{pid}/stat"))?;
        let stat = String::from_utf8_lossy(&stat);
        parse_stat(&stat)
    }

    fn parse_stat(stat: &str) -> Result<TerminalGroup, ProcessError> {
        let malformed = || ProcessError::Os("/proc/<pid>/stat was malformed".to_string());
        // `comm` may hold spaces and parentheses; everything after its
        // closing one is space-separated.
        let after_comm = stat.rsplit_once(')').ok_or_else(malformed)?.1;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let field = |i: usize| -> Result<i32, ProcessError> {
            fields
                .get(i)
                .and_then(|f| f.parse().ok())
                .ok_or_else(malformed)
        };
        Ok(TerminalGroup {
            pgid: field(2)?,
            tpgid: field(5)?,
        })
    }

    pub(super) fn argv(pid: u32) -> Result<NonEmpty<String>, ProcessError> {
        let cmdline = read(&format!("/proc/{pid}/cmdline"))?;
        // A zombie has an empty cmdline: the leader is gone in every sense
        // that matters here.
        let args: Vec<String> = cmdline
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        NonEmpty::from_vec(args).ok_or(ProcessError::Gone)
    }
}

/// Decide `pane_pid`'s foreground now, from the OS, once.
pub fn foreground(pane_pid: u32, shells: &Shells) -> Foreground {
    let unrecovered = |reason| Foreground::Unrecovered { reason };
    let group = match os::terminal_group(pane_pid) {
        Ok(group) => group,
        Err(ProcessError::Gone) => return unrecovered(RecoveryFailure::ShellGone),
        Err(ProcessError::Os(message)) => return unrecovered(RecoveryFailure::Os { message }),
    };
    // A group id of zero or below is "no foreground group", not a pid.
    let leader = match u32::try_from(group.tpgid) {
        Ok(pid) if pid > 0 => pid,
        _ => return unrecovered(RecoveryFailure::NoTerminal),
    };
    let argv = match os::argv(leader) {
        Ok(argv) => argv,
        Err(ProcessError::Gone) => return unrecovered(RecoveryFailure::LeaderGone),
        Err(ProcessError::Os(message)) => return unrecovered(RecoveryFailure::Os { message }),
    };
    Foreground::of(
        TerminalHolder {
            at_own_process: group.tpgid == group.pgid,
            argv,
        },
        shells,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn this_process_reads_its_own_argv_exactly() {
        let argv = os::argv(std::process::id()).unwrap();
        let expected: Vec<String> = std::env::args().collect();
        assert_eq!(argv.iter().cloned().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn a_child_with_a_spaced_argument_keeps_it_as_one_argument() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        // `sleep` is enough to prove the NUL split: a `ps`-style whitespace
        // split would also pass for it, so pair it with a process whose argv
        // holds a space.
        let mut spaced = Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let sleep_argv: Vec<String> = os::argv(child.id()).unwrap().into_iter().collect();
        let sh_argv: Vec<String> = os::argv(spaced.id()).unwrap().into_iter().collect();
        child.kill().unwrap();
        child.wait().unwrap();
        spaced.kill().unwrap();
        spaced.wait().unwrap();
        assert_eq!(sleep_argv, ["sleep", "30"]);
        assert_eq!(sh_argv, ["sh", "-c", "sleep 30"]);
    }

    #[test]
    fn an_exited_process_is_gone_for_both_reads() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(os::argv(pid), Err(ProcessError::Gone));
        assert_eq!(os::terminal_group(pid), Err(ProcessError::Gone));
    }

    #[test]
    fn this_test_process_has_a_group() {
        let group = os::terminal_group(std::process::id()).unwrap();
        assert!(group.pgid > 0);
    }

    #[test]
    fn a_gone_shell_is_an_unrecovered_foreground_with_that_reason() {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(
            foreground(pid, &Shells::default()),
            Foreground::Unrecovered {
                reason: RecoveryFailure::ShellGone
            }
        );
    }
}
