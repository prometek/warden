use super::*;

/// Sentinel meaning "no process start time was recorded for this row" —
/// used for historical rows written before start-time tracking existed.
/// `0` is never a real Unix start time in practice (that would be 1970).
pub const UNKNOWN_START_TIME: i64 = 0;

/// Returns the OS-reported start time (seconds since the Unix epoch) of
/// `pid`, or `None` if no such process exists right now.
///
/// This is what lets [`is_process_alive`] tell a still-running process
/// apart from an unrelated process that happens to have reused the same PID
/// after a reboot: PIDs are a small, wrapping namespace recycled by the OS,
/// so a bare "does this PID exist" check is not sufficient for correctness
/// over the lifetime of a persisted run (H1 / crash recovery, Architecture
/// §9). A process's start time is immutable for its whole lifetime, so
/// re-reading it later and comparing against a value captured right after
/// spawn reliably detects PID reuse.
pub fn process_start_time(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    // A single-PID refresh (not `ProcessesToUpdate::All`) keeps this cheap
    // enough to call synchronously from an async context — it's invoked at
    // most once per agent invocation, never in a hot loop.
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.start_time() as i64)
}

/// Checks whether `pid` still refers to the *same* process that was
/// recorded with `expected_start_time` (seconds since epoch), not merely
/// whether some process with that PID currently exists.
///
/// `pid == 0` is always reported not-alive: POSIX `kill(0, ...)` signals
/// the caller's entire process group rather than a single process, so a
/// naive `kill(pid, None)` liveness check against a pid-0 sentinel always
/// (mis)reports "alive" regardless of whether an agent is actually running
/// — this was H1, a real bug in the previous implementation.
///
/// If `expected_start_time` is [`UNKNOWN_START_TIME`] (no start time was
/// ever recorded for this row), falls back to a plain existence check —
/// strictly less safe against PID reuse, and logged as such.
pub fn is_process_alive(pid: u32, expected_start_time: i64) -> bool {
    if pid == 0 {
        return false;
    }

    let Some(actual_start_time) = process_start_time(pid) else {
        return false;
    };

    if expected_start_time == UNKNOWN_START_TIME {
        tracing::warn!(
            pid,
            "checking process liveness without a recorded start time; cannot rule out PID reuse"
        );
        return true;
    }

    actual_start_time == expected_start_time
}

/// Terminates `pid`, but only if it is still the exact process recorded at
/// `expected_start_time` (H1: PID-reuse hardening).
///
/// Deliberately does *not* call [`is_process_alive`] first and then act on
/// that answer: two separate `sysinfo` refreshes (one to check liveness,
/// another later to obtain a handle to kill) would leave a race window
/// between them where the OS could reuse `pid` for an unrelated process,
/// which this function would then signal by mistake. Instead, a *single*
/// refresh produces the exact process handle that is both fingerprint-
/// checked and killed, so there is no gap in which the PID can change
/// identity out from under this call.
///
/// Returns `Ok(())` if the process is already gone, or is no longer the one
/// recorded (fingerprint mismatch) — neither is an error, both just mean
/// there is nothing left to kill. `pid == 0` is always treated as
/// already-gone: see [`is_process_alive`] for why a pid-0 sentinel must
/// never be signalled.
pub fn kill_pid(pid: u32, expected_start_time: i64) -> Result<(), ProcessError> {
    if pid == 0 {
        return Ok(());
    }

    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );

    let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
        // Nothing at this pid right now — already gone, nothing to do.
        return Ok(());
    };

    let actual_start_time = process.start_time() as i64;
    if expected_start_time == UNKNOWN_START_TIME {
        // Degraded case, same as `is_process_alive`: no fingerprint was ever
        // recorded for this row, so PID reuse can't be ruled out. Logged,
        // not refused — a historical row shouldn't be permanently
        // unreclaimable just because it predates start-time tracking.
        tracing::warn!(
            pid,
            "killing a process without a recorded start time; cannot rule out PID reuse"
        );
    } else if actual_start_time != expected_start_time {
        // The PID has been reused by an unrelated process since it was
        // recorded (H1) — on the very same handle we're about to kill, not
        // a separate, earlier check. Never signal it.
        return Ok(());
    }

    if process.kill() {
        Ok(())
    } else {
        Err(ProcessError::KillFailed { pid })
    }
}
