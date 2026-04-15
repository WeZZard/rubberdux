use std::path::PathBuf;

use tokio::process::Command;

use crate::error::Error;

const SSH_KEY_NAME: &str = "rubberdux_ed25519";

/// Available VM images for provisioning.
pub struct VMImage {
    pub name: &'static str,
    pub oci_image: &'static str,
    pub base_vm_name: &'static str,
}

/// All supported macOS images.
pub const AVAILABLE_IMAGES: &[VMImage] = &[
    VMImage {
        name: "macos15",
        oci_image: "ghcr.io/cirruslabs/macos-sequoia-xcode:latest",
        base_vm_name: "rubberdux-base-macos15",
    },
    VMImage {
        name: "macos26",
        oci_image: "ghcr.io/cirruslabs/macos-tahoe-xcode:latest",
        base_vm_name: "rubberdux-base-macos26",
    },
];

// ---------------------------------------------------------------------------
// Prerequisite checking
// ---------------------------------------------------------------------------

/// Result of checking a single prerequisite.
pub struct PrereqCheck {
    pub name: &'static str,
    pub satisfied: bool,
    pub detail: String,
    pub fix: String,
}

/// Check all prerequisites for VM mode. Returns a list of checks.
pub async fn check_prerequisites() -> Vec<PrereqCheck> {
    let mut checks = Vec::new();

    // 1. macOS check
    checks.push(check_macos());

    // 2. Apple Silicon check
    checks.push(check_apple_silicon().await);

    // 3. Homebrew
    checks.push(check_command("brew", "Homebrew",
        "Install Homebrew: /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"").await);

    // 4. Tart
    checks.push(check_command("tart", "Tart VM manager",
        "Install Tart: brew install cirruslabs/cli/tart").await);

    // 5. sshpass (for initial VM provisioning)
    checks.push(check_command("sshpass", "sshpass",
        "Install sshpass: brew install sshpass").await);

    // 6. Base images
    checks.extend(check_base_images().await);

    // 7. SSH key
    checks.push(check_ssh_key());

    checks
}

/// Print prerequisite status to stdout. Returns true if all pass.
pub fn print_prerequisites(checks: &[PrereqCheck]) -> bool {
    let mut all_ok = true;

    println!("\nRubberdux VM Prerequisites\n");
    for check in checks {
        let icon = if check.satisfied { "ok" } else { "MISSING" };
        println!("  [{}] {} — {}", icon, check.name, check.detail);
        if !check.satisfied {
            println!("        Fix: {}", check.fix);
            all_ok = false;
        }
    }
    println!();

    if all_ok {
        println!("All prerequisites satisfied. VM mode is ready.");
    } else {
        println!("Run `rubberdux setup` to install missing dependencies.");
    }

    all_ok
}

fn check_macos() -> PrereqCheck {
    let is_macos = cfg!(target_os = "macos");
    PrereqCheck {
        name: "macOS",
        satisfied: is_macos,
        detail: if is_macos {
            "Running on macOS".into()
        } else {
            "Not macOS — Tart requires macOS".into()
        },
        fix: "Tart VMs are only supported on macOS with Apple Silicon.".into(),
    }
}

async fn check_apple_silicon() -> PrereqCheck {
    let output = Command::new("uname").arg("-m").output().await;
    let is_arm = output
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "arm64")
        .unwrap_or(false);
    PrereqCheck {
        name: "Apple Silicon",
        satisfied: is_arm,
        detail: if is_arm {
            "arm64 architecture detected".into()
        } else {
            "Not arm64 — Tart requires Apple Silicon".into()
        },
        fix: "Tart requires Apple Silicon (M1/M2/M3/M4).".into(),
    }
}

async fn check_command(cmd: &str, name: &'static str, fix: &str) -> PrereqCheck {
    let found = Command::new("which")
        .arg(cmd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    let detail = if found {
        let version = Command::new(cmd)
            .arg("--version")
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if version.is_empty() {
            format!("{} found", cmd)
        } else {
            version.lines().next().unwrap_or("").to_string()
        }
    } else {
        format!("{} not found", cmd)
    };

    PrereqCheck {
        name,
        satisfied: found,
        detail,
        fix: fix.into(),
    }
}

async fn check_base_images() -> Vec<PrereqCheck> {
    let mut checks = Vec::new();
    for image in AVAILABLE_IMAGES {
        let exists = Command::new("tart")
            .args(["get", image.base_vm_name])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        checks.push(PrereqCheck {
            name: "Base VM image",
            satisfied: exists,
            detail: if exists {
                format!("'{}' exists", image.base_vm_name)
            } else {
                format!("'{}' not found", image.base_vm_name)
            },
            fix: format!(
                "Run `rubberdux setup {}` to provision this image.",
                image.name
            ),
        });
    }
    checks
}

fn check_ssh_key() -> PrereqCheck {
    let key_path = ssh_key_path();
    let exists = key_path.exists();
    PrereqCheck {
        name: "SSH key",
        satisfied: exists,
        detail: if exists {
            format!("{}", key_path.display())
        } else {
            "No SSH key for VM access".into()
        },
        fix: "Run `rubberdux setup` to generate SSH keys.".into(),
    }
}

// ---------------------------------------------------------------------------
// Setup (install missing dependencies)
// ---------------------------------------------------------------------------

/// Run setup for a specific image, or all images if None.
pub async fn run_setup(image_name: Option<&str>) -> Result<(), Error> {
    println!("Rubberdux VM Setup\n");

    // Resolve which images to provision
    let images: Vec<&VMImage> = match image_name {
        Some(name) => {
            let img = AVAILABLE_IMAGES.iter().find(|i| i.name == name);
            match img {
                Some(i) => vec![i],
                None => {
                    let available: Vec<_> = AVAILABLE_IMAGES.iter().map(|i| i.name).collect();
                    return Err(Error::Vm(format!(
                        "Unknown image '{}'. Available: {:?}",
                        name, available
                    )));
                }
            }
        }
        None => AVAILABLE_IMAGES.iter().collect(),
    };

    // Step 1: Check platform
    if !cfg!(target_os = "macos") {
        return Err(Error::Vm(
            "VM mode requires macOS with Apple Silicon.".into(),
        ));
    }

    // Step 2: Ensure Homebrew
    if !command_exists("brew").await {
        println!("[1/6] Installing Homebrew...");
        run_shell("curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh | bash").await?;
    } else {
        println!("[1/6] Homebrew — already installed");
    }

    // Step 3: Ensure Tart
    if !command_exists("tart").await {
        println!("[2/6] Installing Tart...");
        run_shell("brew install cirruslabs/cli/tart").await?;
    } else {
        println!("[2/6] Tart — already installed");
    }

    // Step 4: Ensure sshpass
    if !command_exists("sshpass").await {
        println!("[3/6] Installing sshpass...");
        run_shell("brew install sshpass").await?;
    } else {
        println!("[3/6] sshpass — already installed");
    }

    // Step 5: SSH key
    let key_path = ssh_key_path();
    if !key_path.exists() {
        println!("[4/6] Generating SSH key...");
        ensure_ssh_key()?;
    } else {
        println!("[4/6] SSH key — already exists");
    }

    // Step 6: Pull and provision each image
    for (idx, image) in images.iter().enumerate() {
        let step = format!("[{}/{}]", 5 + idx, 4 + images.len());

        // Pull OCI image if needed
        let oci_exists = Command::new("tart")
            .args(["get", image.oci_image])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !oci_exists {
            println!("{} Pulling {}...", step, image.oci_image);
            println!("       (this may take 20-40 minutes on first run)");
            let status = Command::new("tart")
                .args(["pull", image.oci_image])
                .status()
                .await
                .map_err(|e| Error::Vm(format!("tart pull failed: {}", e)))?;
            if !status.success() {
                return Err(Error::Vm(format!("tart pull {} failed", image.oci_image)));
            }
        } else {
            println!("{} {} — already pulled", step, image.oci_image);
        }

        // Provision base VM if needed
        let base_exists = Command::new("tart")
            .args(["get", image.base_vm_name])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !base_exists {
            println!("       Provisioning {}...", image.base_vm_name);
            provision_base_vm(image).await?;
        } else {
            println!("       {} — already provisioned", image.base_vm_name);
        }
    }

    println!("\nSetup complete. You can now run `rubberdux --host`.");
    Ok(())
}

/// Provision the base VM from the OCI image.
async fn provision_base_vm(image: &VMImage) -> Result<(), Error> {
    let tmp_name = format!("{}-building", image.base_vm_name);

    // Clone from OCI image
    println!("       Cloning base image...");
    let status = Command::new("tart")
        .args(["clone", image.oci_image, &tmp_name])
        .status()
        .await
        .map_err(|e| Error::Vm(format!("tart clone failed: {}", e)))?;
    if !status.success() {
        return Err(Error::Vm("tart clone failed".into()));
    }

    // Set VM resources: match host CPU, 5/8 of host memory
    let cpu_count = num_cpus();
    let mem_mb = (total_memory_mb() * 5) / 8;
    println!(
        "       Configuring VM: {} CPUs, {} MB RAM...",
        cpu_count, mem_mb
    );

    let _ = Command::new("tart")
        .args(["set", &tmp_name, "--cpu", &cpu_count.to_string()])
        .status()
        .await;
    let _ = Command::new("tart")
        .args(["set", &tmp_name, "--memory", &mem_mb.to_string()])
        .status()
        .await;

    // Boot the VM, run provisioning via shared directory, then stop
    println!("       Booting VM for provisioning...");
    let provision_dir = provision_dir();
    tokio::fs::create_dir_all(&provision_dir).await?;

    // Write the install script to the provision directory
    let install_script = include_str!("guest/install.sh");
    tokio::fs::write(provision_dir.join("install.sh"), install_script).await?;

    // Write the SSH public key
    let pub_key_path = ssh_key_path().with_extension("pub");
    if pub_key_path.exists() {
        let pub_key = tokio::fs::read_to_string(&pub_key_path).await?;
        tokio::fs::write(provision_dir.join("authorized_keys"), pub_key).await?;
    }

    // Copy rubberdux binary to provision directory
    let exe_path = std::env::current_exe()
        .map_err(|e| Error::Vm(format!("failed to get current exe: {}", e)))?;
    tokio::fs::copy(&exe_path, provision_dir.join("rubberdux")).await?;

    // Start VM with shared directory
    let share_arg = format!("__provision:{}", provision_dir.display());
    let mut vm_proc = Command::new("tart")
        .args(["run", &tmp_name, "--no-graphics", "--dir", &share_arg])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::Vm(format!("failed to start VM: {}", e)))?;

    // Wait for IP
    println!("       Waiting for VM network...");
    let ip = wait_for_ip(&tmp_name).await?;
    println!("       VM IP: {}", ip);

    // Wait for SSH
    println!("       Waiting for SSH...");
    wait_for_ssh(&ip).await?;

    // Run install script inside VM via SSH
    // The default user on cirruslabs images is 'admin' with password 'admin'
    println!("       Running install script inside VM...");
    let install_result = Command::new("sshpass")
        .args([
            "-p", "admin",
            "ssh",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            &format!("admin@{}", ip),
            "bash '/Volumes/My Shared Files/__provision/install.sh'",
        ])
        .output()
        .await
        .map_err(|e| Error::Vm(format!("ssh install failed: {}", e)))?;

    if !install_result.status.success() {
        let stderr = String::from_utf8_lossy(&install_result.stderr);
        println!("       Install script output:");
        println!("{}", String::from_utf8_lossy(&install_result.stdout));
        return Err(Error::Vm(format!("install script failed: {}", stderr)));
    }
    println!("       Install script completed.");

    // Stop the VM
    println!("       Stopping VM...");
    let _ = Command::new("tart")
        .args(["stop", &tmp_name])
        .status()
        .await;

    // Wait for the process to exit
    let _ = vm_proc.wait().await;

    // Rename to final name
    let _ = Command::new("tart")
        .args(["rename", &tmp_name, image.base_vm_name])
        .status()
        .await;

    println!("       Base VM '{}' ready.", image.base_vm_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn run_shell(cmd: &str) -> Result<(), Error> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .await
        .map_err(|e| Error::Vm(format!("command failed: {}", e)))?;
    if !status.success() {
        return Err(Error::Vm(format!("command failed: {}", cmd)));
    }
    Ok(())
}

fn ssh_key_path() -> PathBuf {
    dirs_home().join(".ssh").join(SSH_KEY_NAME)
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn provision_dir() -> PathBuf {
    dirs_home()
        .join(".rubberdux")
        .join("vm-provision")
}

fn ensure_ssh_key() -> Result<(), Error> {
    let key_path = ssh_key_path();
    let ssh_dir = key_path.parent().unwrap();
    std::fs::create_dir_all(ssh_dir)
        .map_err(|e| Error::Vm(format!("failed to create ~/.ssh: {}", e)))?;

    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t", "ed25519",
            "-f", &key_path.to_string_lossy(),
            "-N", "",
            "-q",
            "-C", "rubberdux-vm",
        ])
        .status()
        .map_err(|e| Error::Vm(format!("ssh-keygen failed: {}", e)))?;

    if !status.success() {
        return Err(Error::Vm("ssh-keygen failed".into()));
    }

    println!("       Generated {}", key_path.display());
    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn total_memory_mb() -> usize {
    // Read from sysctl on macOS
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .ok()
        })
        .map(|bytes| bytes / (1024 * 1024))
        .unwrap_or(8192)
}

async fn wait_for_ip(vm_name: &str) -> Result<String, Error> {
    for _ in 0..30 {
        let output = Command::new("tart")
            .args(["ip", vm_name])
            .output()
            .await;
        if let Ok(o) = output {
            let ip = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !ip.is_empty() && o.status.success() {
                return Ok(ip);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(Error::Vm(format!(
        "VM {} did not get an IP after 60 seconds",
        vm_name
    )))
}

async fn wait_for_ssh(ip: &str) -> Result<(), Error> {
    // Cirrus Labs images use admin/admin for initial access
    for _ in 0..60 {
        let result = Command::new("sshpass")
            .args([
                "-p", "admin",
                "ssh",
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=5",
                &format!("admin@{}", ip),
                "true",
            ])
            .output()
            .await;

        if let Ok(o) = result {
            if o.status.success() {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(Error::Vm("SSH not ready after 120 seconds".into()))
}

pub fn get_image(name: &str) -> Option<&'static VMImage> {
    AVAILABLE_IMAGES.iter().find(|i| i.name == name)
}

pub fn ssh_private_key() -> PathBuf {
    ssh_key_path()
}
