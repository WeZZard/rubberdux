//! VC-5.1 / VC-5.2 — the DURABILITY integration sink for the append boundary,
//! composed over `src/agent/world/event_log.rs` alone. It touches no
//! `src/agent/world/*` file; it pins the contract the `append`/`append_jsonl`
//! fsync (DU-fsync) already implements.
//!
//! - **VC-5.1 (`append` `sync_data`'s before it acks)** — genuinely coupled to the
//!   fsync via the additive sync-observation seam
//!   (`FilesystemEventLog::with_sync_observer`). A spy records every `sync_data`
//!   that COMPLETED before its `append` returned `Ok`; after a synchronous
//!   `append` returns, the spy has fired exactly once, proving the fsync ran
//!   before the ack. The spy is fused with the `sync_data` call in
//!   `append_jsonl_observing_sync` (`file.sync_data().map(|()| on_sync())?`), so
//!   deleting or reordering the fsync makes the spy NOT fire and this test goes
//!   RED — that fusion is what makes the proof non-vacuous. (The seam is a no-op
//!   in production, so the on-disk bytes are unchanged: the byte-identity sinks
//!   stay green.)
//! - **VC-5.2 (crash-injection — durable across a process crash)** — a genuine
//!   subprocess child (this same test binary, re-exec'd in a CHILD role) opens the
//!   log, `append`s the Input, then `std::process::abort()`s — a real crash with NO
//!   destructor, NO graceful flush, NO close. The parent waits for the child,
//!   confirms it died by SIGABRT (not a clean exit), then opens a FRESH
//!   `FilesystemEventLog` over the same path and asserts the Input survived. This
//!   proves durability lives in `append`'s RETURN, not in any teardown: the record
//!   outlives a hard process crash that ran no flush/close. (Scope: this is a
//!   same-machine process crash + missing-teardown proof — it does NOT, and cannot,
//!   prove power-loss or device-level durability, since a same-host reload reads the
//!   kernel page cache regardless. The power-loss guarantee is the `sync_data`
//!   syscall's contract, witnessed by VC-5.1's coupling, not by this reload.)
//!
//! Both tests are deterministic and offline — they make no model call and use a
//! per-run `tempfile` directory, so they run on every developer machine without
//! credentials. See docs/agent/world/ecs-runtime.md §"APPEND FSYNC".

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rubberdux::agent::world::event_log::{EventLog, FilesystemEventLog};
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};

/// Env var that flips this test binary's `vc_5_2_*` function into its CHILD role.
/// When set, the function opens the log named by its value, appends the Input, and
/// aborts — modelling the process vanishing right after `append` returns. The
/// parent sets it on the spawned child only, so a normal suite run (where the var
/// is unset) always takes the PARENT path and drives a real subprocess crash.
const CHILD_ENV: &str = "RUBBERDUX_APPEND_DURABILITY_CHILD";

/// The one Input whose durability across an abort-after-append this sink pins. A
/// fixed value keeps the test fully deterministic; per-run isolation comes from a
/// fresh `tempfile` directory, not from a unique payload.
const DURABLE_TEXT: &str = "durable-before-ack: this Input must survive an abort-after-append";

/// The single Event both roles agree on. Round-trips through JSONL byte-identically
/// (`UserMessage` carries only `to`/`text`), so a post-crash `load` yields exactly
/// this value when — and only when — the append reached durable storage.
fn durable_input() -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at: 1,
        wall: None,
        input: LogicalInput::UserMessage {
            to: 0,
            text: DURABLE_TEXT.into(),
        },
    }
}

/// The exact `--exact` libtest filter that runs ONLY the `vc_5_2_*` function in the
/// re-exec'd child. `module_path!()` prepends the crate-name segment that libtest
/// test names omit, so strip it to recover the test's libtest name.
fn crash_child_libtest_name() -> String {
    let module = module_path!();
    let without_crate = module
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or(module);
    format!("{without_crate}::vc_5_2_crash_injection_abort_after_append_survives_reload")
}

// ---------------------------------------------------------------------------
// VC-5.1 — append invokes sync_data BEFORE returning Ok (genuine fsync coupling)
// ---------------------------------------------------------------------------

/// **VC-5.1** `FilesystemEventLog::append` performs its durable `sync_data` BEFORE
/// returning `Ok`. We inject a sync spy through the additive observation seam
/// (`with_sync_observer`); the seam fires the spy fused with the `sync_data` call
/// (`file.sync_data().map(|()| on_sync())?`), so the spy fires if and only if the
/// fsync itself succeeds. After each synchronous `append` returns, the spy count
/// has advanced by exactly one — the fsync ran during the call, before the ack.
///
/// This is GENUINELY coupled to the fsync: if the `sync_data` call were removed or
/// moved after the `Ok`, the spy would never fire and the `== 1` / `== 2`
/// assertions below would FAIL. A `write_all`-into-page-cache without an fsync —
/// which a same-host fresh-handle reload cannot distinguish — does NOT advance the
/// spy, so this test pins the fsync specifically, not merely write-before-ack.
///
/// The fresh-handle reload at the end re-confirms the bytes are observable, but the
/// load-bearing durability claim here is the spy: append returns ⇒ `sync_data` has
/// already run.
#[test]
fn vc_5_1_append_invokes_sync_data_before_returning_ok() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("world-events.jsonl");

    // The spy: each completed `sync_data` bumps this counter. It is shared with the
    // log through the seam; the test reads it back across `append` calls.
    let syncs = Arc::new(AtomicUsize::new(0));
    let spy: Box<dyn Fn() + Send + Sync> = {
        let syncs = Arc::clone(&syncs);
        Box::new(move || {
            syncs.fetch_add(1, Ordering::SeqCst);
        })
    };
    let mut log = FilesystemEventLog::with_sync_observer(&path, spy);

    // Before any append, no fsync has happened.
    assert_eq!(
        syncs.load(Ordering::SeqCst),
        0,
        "no sync_data may fire before the first append",
    );

    let first = durable_input();
    log.append(&first).expect("append returns Ok (post-sync_data)");
    // `append` returned synchronously; the spy fired during it. Because the spy is
    // fused with `sync_data`'s success, count == 1 proves the fsync completed
    // BEFORE this `append` returned `Ok`. Remove the fsync ⇒ count stays 0 ⇒ RED.
    assert_eq!(
        syncs.load(Ordering::SeqCst),
        1,
        "append must sync_data exactly once, before returning Ok",
    );

    let second = Event {
        at: 2,
        ..durable_input()
    };
    log.append(&second).expect("second append returns Ok (post-sync_data)");
    // Each append fsyncs independently before its own ack — not a one-off or a
    // close-time flush. Drop the fsync ⇒ this stalls at 1 ⇒ RED.
    assert_eq!(
        syncs.load(Ordering::SeqCst),
        2,
        "every append performs exactly one durable sync_data before its ack",
    );

    // A fresh handle re-confirms the synced bytes are readable. (On a same host this
    // reload alone proves only write-before-ack, not the fsync — the spy above is
    // what pins the fsync.)
    let reloaded = FilesystemEventLog::new(&path).load().expect("reload fresh handle");
    assert_eq!(
        reloaded,
        vec![first, second],
        "both synced Inputs are observable on a fresh handle with no caller flush",
    );
}

// ---------------------------------------------------------------------------
// VC-5.2 — abort-after-append; the Input survives on reload (crash-injection)
// ---------------------------------------------------------------------------

/// **VC-5.2** A genuine crash-injection. Re-exec'd with `CHILD_ENV` set, this
/// function takes the CHILD role: open the log, `append` the Input, then
/// `std::process::abort()` — the process vanishes with NO destructor, NO flush, NO
/// graceful close. The PARENT spawns that child as a real subprocess, waits for it,
/// confirms it died by SIGABRT (a real abort, not a clean exit), then opens a FRESH
/// `FilesystemEventLog` over the same path and asserts the Input survived.
///
/// What this proves: durability lives in `append`'s RETURN, not in any teardown.
/// The Input survives a hard process crash that ran no graceful flush/close
/// precisely because `append` `sync_data`'d before returning `Ok` — there was
/// nothing left for a later close to commit. What this does NOT prove: power-loss
/// or device-level durability. A same-host reload reads the kernel page cache, so
/// this test cannot, on its own, distinguish a real fsync from a bare `write_all`;
/// the power-loss guarantee is the `sync_data` syscall's contract, witnessed by
/// VC-5.1's fsync coupling rather than by this reload.
#[test]
fn vc_5_2_crash_injection_abort_after_append_survives_reload() {
    // CHILD ROLE — the re-exec'd subprocess. Append, then abort with no cleanup.
    if let Ok(path) = std::env::var(CHILD_ENV) {
        let mut log = FilesystemEventLog::new(&path);
        log.append(&durable_input())
            .expect("child: append returns Ok (post-sync_data)");
        // Vanish right after `append` returns — no flush, no close, no Drop runs.
        std::process::abort();
    }

    // PARENT ROLE — drive the crash and verify durability across it.
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("world-events.jsonl");

    let exe = std::env::current_exe().expect("path to this test binary");
    let status = Command::new(exe)
        .arg(crash_child_libtest_name())
        .arg("--exact")
        .arg("--test-threads=1")
        .arg("--quiet")
        .env(CHILD_ENV, &path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn the crash-injection child subprocess");

    // The child genuinely CRASHED: it did not exit cleanly. A clean exit (e.g. a
    // mis-targeted filter running zero tests) would invalidate the crash injection,
    // so this guards the proof's premise.
    assert!(
        !status.success(),
        "the child must abort, not exit cleanly — a clean exit means no crash was injected",
    );
    // On unix a `std::process::abort()` delivers SIGABRT (6): a hard crash that ran
    // no Rust destructor and no graceful close — the strongest evidence the append
    // was acknowledged with nothing left to flush.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(6),
            "the child died by SIGABRT — a genuine std::process::abort(), not a graceful shutdown",
        );
    }

    // Open a FRESH `FilesystemEventLog` over the SAME path. The Input the child
    // appended-then-aborted-on is present precisely because `append` returned only
    // after fsync'ing — durability is in the ack, surviving a crash with no teardown.
    let reopened = FilesystemEventLog::new(&path);
    let survived = reopened.load().expect("reload the log after the crash");
    assert_eq!(
        survived,
        vec![durable_input()],
        "the abort-after-append Input survives a hard process crash — durability is in append's return",
    );
}
