//! Ending a client's process, for the force-close binding.
//!
//! Every other way a window closes here is a request: `xdg_toplevel.close`,
//! `WM_DELETE_WINDOW`. A request needs a client that is still reading its
//! socket, and the client somebody most needs rid of is the one that is not —
//! a game that has locked the pointer and then hung holds the session's mouse
//! until its process ends, and nothing polite will end it.
//!
//! So: `SIGTERM` at once, which a live client can still catch and tidy up
//! under, and `SIGKILL` after [`GRACE`] if the process is still there.
//!
//! The wait is a sleeping thread rather than a calloop timer for the reason
//! [`reap`](crate::backend::reap) is one: this happens a handful of times a
//! session, and the thread needs nothing from the compositor's state — only a
//! PID and the kernel.

use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};

/// How long a process gets between `SIGTERM` and `SIGKILL`.
///
/// Long enough for a client that handles the signal to flush and leave, short
/// enough that somebody whose pointer is held is not left counting.
const GRACE: Duration = Duration::from_secs(2);

/// Terminate `pid`, and kill it if it is still running after [`GRACE`].
///
/// Refuses the PIDs that are never a window's client and would be a disaster
/// if something upstream ever reported them as one: 0 (which `kill(2)` reads
/// as "my whole process group"), init, and the compositor itself.
pub(crate) fn terminate(pid: u32) {
    let Some(target) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        tracing::warn!(pid, "force close: not a PID");
        return;
    };
    if pid <= 1 || pid == std::process::id() {
        tracing::warn!(pid, "force close: refusing to signal this PID");
        return;
    }

    // Taken before the first signal, so the thread below can tell the process
    // it was asked to kill from a stranger that inherited the number.
    let started = start_time(pid);
    if let Err(e) = kill_process(target, Signal::TERM) {
        tracing::warn!(pid, error = %e, "force close: SIGTERM failed");
        return;
    }
    tracing::info!(pid, "force close: sent SIGTERM");

    let spawned = std::thread::Builder::new()
        .name("huginn-kill".to_string())
        .stack_size(64 * 1024)
        .spawn(move || {
            std::thread::sleep(GRACE);
            // Gone, or gone and the PID reused: either way not ours to kill.
            // A zombie still has its entry and its start time, and killing one
            // is harmless — it is already dead and waiting to be reaped.
            let now = start_time(pid);
            if now.is_none() || now != started {
                return;
            }
            match kill_process(target, Signal::KILL) {
                Ok(()) => tracing::info!(pid, "force close: SIGTERM ignored, sent SIGKILL"),
                Err(e) => tracing::warn!(pid, error = %e, "force close: SIGKILL failed"),
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(pid, error = %e, "force close: no thread for the SIGKILL follow-up");
    }
}

/// When `pid` started, in clock ticks since boot, or `None` if it is gone.
///
/// A PID alone does not name a process for longer than the process lives; a
/// PID and a start time does.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_time(&stat)
}

/// Field 22 of `/proc/<pid>/stat`.
///
/// Counted from the *last* `)`: field 2 is the command name in parentheses,
/// and a command name may contain spaces and parentheses of its own.
fn parse_start_time(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts at field 3, so field 22 is the twentieth of what is left.
    rest.split_ascii_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_time_is_read_past_an_awkward_command_name() {
        let stat = "4242 (Skyrim) SE.exe) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    5 6 0 0 20 0 9 0 987654 123456 789 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(987_654));
    }

    #[test]
    fn a_truncated_stat_line_has_no_start_time() {
        assert_eq!(parse_start_time("4242 (x) S 1 2 3"), None);
        assert_eq!(parse_start_time(""), None);
    }

    #[test]
    fn this_process_has_a_start_time_and_a_missing_one_does_not() {
        assert!(start_time(std::process::id()).is_some());
        // Above the kernel's hard PID ceiling of 2^22, so never a process.
        assert_eq!(start_time(u32::MAX), None);
    }

    #[test]
    fn a_sleeping_child_is_terminated() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        terminate(child.id());
        let status = child.wait().expect("wait for sleep");
        assert!(!status.success(), "sleep ran to completion: {status:?}");
    }
}
