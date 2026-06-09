//! Git hook arming: points `core.hooksPath` at the tracked `.githooks/`
//! directory so the design-documentation linter runs as a `pre-commit` hook.
//!
//! Contract:
//! - `install()` arms the current clone idempotently and reports what it did —
//!   the documented onboarding command, run once per clone by any contributor
//!   (including documentation-only contributors who never build).
//! - `ensure_armed_quietly()` performs the same arming as a best-effort, silent
//!   guard at the start of every `cargo xtask` invocation, so contributors who
//!   build are armed without a manual step. It never fails its caller.
//!
//! Both are no-ops outside a git work tree.
//!
//! Rationale: git never arms hooks on clone (a security decision) and
//! `.git/hooks` is untracked, so the hook must be shared through a tracked
//! directory referenced by `core.hooksPath`. A local hook is bypassable
//! (`--no-verify`) and only as present as each clone arms it, so CI runs the
//! same `cargo xtask lint` as the authoritative, non-bypassable gate.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The tracked directory holding shared hooks, relative to the work-tree root.
const HOOKS_DIR: &str = ".githooks";

/// Explicit, verbose arm step — the documented onboarding command.
pub fn install() -> Result<(), String> {
    let root = repo_toplevel().ok_or("not inside a git work tree")?;
    if current_hooks_path(&root).as_deref() == Some(HOOKS_DIR) {
        println!("git hooks already armed (core.hooksPath={HOOKS_DIR})");
    } else {
        set_hooks_path(&root)?;
        println!("armed git hooks: core.hooksPath={HOOKS_DIR}");
    }
    ensure_executable(&root)?;
    Ok(())
}

/// Best-effort, silent guard run before every `cargo xtask` command. Arms the
/// clone if it is not already armed; swallows every error so it can never block
/// the command the developer actually asked for.
pub fn ensure_armed_quietly() {
    let Some(root) = repo_toplevel() else {
        return;
    };
    if current_hooks_path(&root).as_deref() == Some(HOOKS_DIR) {
        return;
    }
    if set_hooks_path(&root).is_ok() {
        let _ = ensure_executable(&root);
        println!("xtask: armed git pre-commit hook (core.hooksPath={HOOKS_DIR})");
    }
}

/// The work-tree root, or `None` when not inside a git work tree.
fn repo_toplevel() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// The clone's current local `core.hooksPath`, or `None` when unset.
fn current_hooks_path(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["config", "--local", "--get", "core.hooksPath"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Point `core.hooksPath` at the tracked hooks directory for this clone.
fn set_hooks_path(root: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .current_dir(root)
        .args(["config", "--local", "core.hooksPath", HOOKS_DIR])
        .status()
        .map_err(|e| format!("git config failed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("git config core.hooksPath failed".into())
    }
}

/// Restore the executable bit on the tracked hook, which a clone can drop.
fn ensure_executable(root: &Path) -> Result<(), String> {
    let hook = root.join(HOOKS_DIR).join("pre-commit");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&hook) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms)
                .map_err(|e| format!("chmod {}: {e}", hook.display()))?;
        }
    }
    #[cfg(not(unix))]
    let _ = &hook;
    Ok(())
}
