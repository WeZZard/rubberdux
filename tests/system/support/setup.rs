use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

static BUILT_BINARY: OnceLock<PathBuf> = OnceLock::new();
static LINUX_BINARY: OnceLock<PathBuf> = OnceLock::new();

/// Build the release binary once and return its path.
pub fn release_binary_path() -> &'static Path {
    BUILT_BINARY.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = manifest_dir
            .join("target")
            .join("release")
            .join("rubberdux");

        let status = std::process::Command::new("cargo")
            .args(["build"])
            .current_dir(&manifest_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("cargo build failed");

        assert!(status.success(), "cargo build exited with error");
        assert!(
            target_dir.exists(),
            "release binary not found at {:?}",
            target_dir
        );
        target_dir
    })
}

/// Return the path to the Linux agent binary (cross-compiled).
/// Panics with helpful instructions if the binary doesn't exist.
pub fn linux_agent_binary_path() -> &'static Path {
    LINUX_BINARY.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let linux_binary = manifest_dir
            .join("target")
            .join("linux-agent-build")
            .join("aarch64-unknown-linux-musl")
            .join("release")
            .join("rubberdux");

        if !linux_binary.exists() {
            panic!(
                "Linux agent binary not found at {}.\n\
                The build.rs script should have built it automatically.\n\
                Run `cargo build` to trigger the build process.",
                linux_binary.display()
            );
        }
        linux_binary
    })
}

/// Force-delete any stale Tart VMs left behind by prior crashed test runs.
pub fn cleanup_stale_vms() {
    let output = std::process::Command::new("tart")
        .args(["list"])
        .output()
        .expect("tart list failed");

    if !output.status.success() {
        eprintln!("Warning: tart list failed during cleanup sweep");
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Lines look like: "local  rubberdux-main-42  140  83   5日前     stopped"
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2
            && parts[1].starts_with("rubberdux-")
            && !parts[1].starts_with("rubberdux-base-")
        {
            let name = parts[1];
            let _ = std::process::Command::new("tart")
                .args(["stop", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = std::process::Command::new("tart")
                .args(["delete", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
