use super::*;

/// Sentinel meaning "no process start time was recorded for this row" — used for historical rows
/// written before start-time tracking existed.
pub const UNKNOWN_START_TIME: i64 = 0;

/// Returns the OS-reported start time (seconds since the Unix epoch) of `pid`, or `None` if no such
/// process exists right now.
pub fn process_start_time(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.start_time() as i64)
}

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

/// Terminates `pid`, but only if it is still the exact process recorded at `expected_start_time`
/// (H1: PID-reuse hardening).
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
        // Degraded case, same as `is_process_alive`: no fingerprint was ever recorded for this row,
        // so PID reuse can't be ruled out.
        tracing::warn!(
            pid,
            "killing a process without a recorded start time; cannot rule out PID reuse"
        );
    } else if actual_start_time != expected_start_time {
        // The PID has been reused by an unrelated process since it was recorded (H1) — on the very
        // same handle we're about to kill, not a separate, earlier check.
        return Ok(());
    }

    if process.kill() {
        Ok(())
    } else {
        Err(ProcessError::KillFailed { pid })
    }
}
