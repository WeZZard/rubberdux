//! Shared helpers for the App subprocess system cases: observing a worker's
//! entry stream and locating/crashing its OS process by the `--task-id` it was
//! spawned with.

use std::time::Duration;

use rubberdux::agent::runtime::port::EntryNotification;
use tokio::sync::broadcast;

/// Wait for the next entry to arrive on an App's entry stream, returning `true`
/// if one arrives within the deadline. A `Lagged` is treated as "an entry
/// happened" (the buffer advanced past our cursor), which is the signal we
/// want; a closed channel returns `false`.
pub async fn await_entry(
    receiver: &mut broadcast::Receiver<EntryNotification>,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, async {
        // A delivered entry or a `Lagged` both mean an entry happened (the
        // buffer advanced past our cursor); only a closed channel is a failure.
        match receiver.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => true,
            Err(broadcast::error::RecvError::Closed) => false,
        }
    })
    .await
    .unwrap_or(false)
}

/// Find the PIDs of worker subprocesses spawned for `app_id`. Workers are
/// spawned with `--task-id <app_id>` on their command line, so a process-table
/// scan for that exact argument pair identifies them without the supervisor
/// having to expose child PIDs.
pub fn worker_pids(app_id: &str) -> Vec<u32> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,args="])
        .output();
    let stdout = match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => return Vec::new(),
    };
    let needle = format!("--task-id {app_id}");
    stdout
        .lines()
        .filter(|line| line.contains("--agent") && line.contains(&needle))
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let pid_str = trimmed.split_whitespace().next()?;
            pid_str.parse::<u32>().ok()
        })
        .collect()
}

/// Hard-kill a worker process by PID (the crash the supervisor must recover
/// from). Best-effort: a process that already exited yields a harmless error.
pub fn kill_worker_process(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}
