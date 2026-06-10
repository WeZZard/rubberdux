//! The App runtime layer: how an App's agent worker actually runs.
//!
//! An App's worker can run in-process (the `MemorySupervisor` in
//! `crate::app::supervisor`) or as a separate OS process that bridges its
//! `AgentLoop` to the host over the RPC protocol. This module hosts the
//! out-of-process realization's worker entry point; the supervisor that *spawns*
//! that process is designed and implemented separately. The lifecycle rationale
//! is in `docs/app/runtime/worker-lifecycle.md`.

// The subprocess supervisor reuses the host's `Hello`-routed accept path
// (`crate::host`), which is itself gated on the `host` feature; gate the
// supervisor the same way so an agent-only build still compiles.
pub mod lifecycle;
#[cfg(feature = "host")]
pub mod local_supervisor;
pub mod worker;
pub mod worker_handle;
