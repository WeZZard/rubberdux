//! The App runtime layer: how an App's agent worker actually runs.
//!
//! An App's worker can run in-process (the `MemorySupervisor` in
//! `crate::app::supervisor`) or as a separate OS process that bridges its
//! `AgentLoop` to the host over the RPC protocol. This module hosts the
//! out-of-process realization's worker entry point; the supervisor that *spawns*
//! that process is designed and implemented separately. The lifecycle rationale
//! is in `docs/app/runtime/worker-lifecycle.md`.

pub mod worker;
