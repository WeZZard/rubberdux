use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

const IMAGES: &[(&str, &str, &str)] = &[
    (
        "ubuntu24",
        "ghcr.io/cirruslabs/ubuntu:24.04",
        "rubberdux-base-ubuntu24-release",
    ),
    (
        "macos15",
        "ghcr.io/cirruslabs/macos-sequoia-xcode:latest",
        "rubberdux-base-macos15-release",
    ),
    (
        "macos26",
        "ghcr.io/cirruslabs/macos-tahoe-xcode:latest",
        "rubberdux-base-macos26-release",
    ),
];

pub async fn provision_images(image: Option<String>) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        println!("Skipping VM provisioning on non-macOS platform");
        return Ok(());
    }

    if !command_exists("tart") {
        return Err("Tart VM manager not installed.\n\
             Install with:\n\
             brew install cirruslabs/cli/tart"
            .into());
    }

    let images: Vec<&(&str, &str, &str)> = match image {
        Some(name) => {
            let found = IMAGES.iter().find(|(n, _, _)| **n == name);
            match found {
                Some(img) => vec![img],
                None => {
                    let available: Vec<_> = IMAGES.iter().map(|(n, _, _)| *n).collect();
                    return Err(format!(
                        "Unknown image '{}'. Available: {:?}",
                        name, available
                    ));
                }
            }
        }
        None => IMAGES.iter().collect(),
    };

    for (image_name, oci_image, base_vm_name) in images {
        if base_vm_up_to_date(base_vm_name)? {
            println!("VM image {} is up to date", base_vm_name);
            continue;
        }

        println!(
            "VM image {} is stale or missing. Reprovisioning...",
            base_vm_name
        );

        // Check if base VM exists - if so, we can clone from it instead of re-downloading OCI image
        let source = if base_vm_exists(base_vm_name) {
            println!(
                "Base VM {} exists - cloning from it (no download needed)",
                base_vm_name
            );
            base_vm_name.to_string()
        } else {
            println!(
                "Base VM {} not found - pulling OCI image {}...",
                base_vm_name, oci_image
            );
            pull_oci_image(oci_image)?;
            oci_image.to_string()
        };

        provision_base_vm(image_name, &source, base_vm_name)?;
    }

    Ok(())
}

fn base_vm_up_to_date(base_vm_name: &str) -> Result<bool, String> {
    let output = Command::new("tart")
        .args(["get", base_vm_name])
        .output()
        .map_err(|e| format!("Failed to check VM {}: {}", base_vm_name, e))?;

    if !output.status.success() {
        return Ok(false);
    }

    let current_hash = compute_provision_hash(base_vm_name)?;
    let stored_hash = read_stored_hash(base_vm_name);

    Ok(stored_hash.as_ref() == Some(&current_hash))
}

fn compute_provision_hash(_base_vm_name: &str) -> Result<String, String> {
    let mut hasher = Sha256::new();

    let software_script = include_str!("../../src/vm/guest/software.sh");
    hasher.update(software_script.as_bytes());

    let install_script = include_str!("../../src/vm/guest/install.sh");
    hasher.update(install_script.as_bytes());

    let binary_path = linux_agent_binary_path();
    let mut file = std::fs::File::open(&binary_path)
        .map_err(|e| format!("Failed to open binary {}: {}", binary_path.display(), e))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut buf)
        .map_err(|e| format!("Failed to read binary {}: {}", binary_path.display(), e))?;
    hasher.update(&buf);

    Ok(hex::encode(hasher.finalize()))
}

fn read_stored_hash(base_vm_name: &str) -> Option<String> {
    let hash_path = provision_hash_path(base_vm_name);
    std::fs::read_to_string(hash_path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn write_stored_hash(base_vm_name: &str, hash: &str) -> Result<(), String> {
    let hash_path = provision_hash_path(base_vm_name);
    if let Some(parent) = hash_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create provision dir: {}", e))?;
    }
    std::fs::write(&hash_path, hash).map_err(|e| format!("Failed to write hash file: {}", e))?;
    Ok(())
}

fn provision_hash_path(base_vm_name: &str) -> PathBuf {
    dirs_home()
        .join(".rubberdux")
        .join("vm-provision")
        .join(format!("{}.hash", base_vm_name))
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn linux_agent_binary_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join("target")
        .join("linux-agent-build")
        .join("aarch64-unknown-linux-musl")
        .join("release")
        .join("rubberdux")
}

fn base_vm_exists(base_vm_name: &str) -> bool {
    Command::new("tart")
        .args(["get", base_vm_name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn pull_oci_image(oci_image: &str) -> Result<(), String> {
    println!(
        "Pulling OCI image {} (this may take 20-40 minutes on first run)...",
        oci_image
    );
    let status = Command::new("tart")
        .args(["pull", oci_image])
        .status()
        .map_err(|e| format!("Failed to pull OCI image {}: {}", oci_image, e))?;
    if !status.success() {
        return Err(format!("Failed to pull OCI image {}", oci_image));
    }
    Ok(())
}

fn provision_base_vm(image_name: &str, source: &str, base_vm_name: &str) -> Result<(), String> {
    let tmp_name = format!("{}-building", base_vm_name);

    println!("Cloning base image for {} from {}...", base_vm_name, source);
    let status = Command::new("tart")
        .args(["clone", source, &tmp_name])
        .status()
        .map_err(|e| format!("tart clone failed: {}", e))?;
    if !status.success() {
        return Err("tart clone failed".into());
    }

    let cpu_count = std::cmp::min(num_cpus(), 6);
    let mem_mb = std::cmp::min((total_memory_mb() * 5) / 8, 8192);

    let _ = Command::new("tart")
        .args(["set", &tmp_name, "--cpu", &cpu_count.to_string()])
        .status();
    let _ = Command::new("tart")
        .args(["set", &tmp_name, "--memory", &mem_mb.to_string()])
        .status();

    if image_name == "ubuntu24" {
        let _ = Command::new("tart")
            .args(["set", &tmp_name, "--disk-size", "50"])
            .status();
    }

    let provision_dir = dirs_home().join(".rubberdux").join("vm-provision");
    std::fs::create_dir_all(&provision_dir)
        .map_err(|e| format!("Failed to create provision dir: {}", e))?;

    let install_script = include_str!("../../src/vm/guest/install.sh");
    std::fs::write(provision_dir.join("install.sh"), install_script)
        .map_err(|e| format!("Failed to write install.sh: {}", e))?;

    let software_script = include_str!("../../src/vm/guest/software.sh");
    std::fs::write(provision_dir.join("software.sh"), software_script)
        .map_err(|e| format!("Failed to write software.sh: {}", e))?;

    let ssh_key_path = dirs_home().join(".ssh").join("rubberdux_ed25519.pub");
    if ssh_key_path.exists() {
        let pub_key = std::fs::read_to_string(&ssh_key_path)
            .map_err(|e| format!("Failed to read SSH public key: {}", e))?;
        std::fs::write(provision_dir.join("authorized_keys"), pub_key)
            .map_err(|e| format!("Failed to write authorized_keys: {}", e))?;
    } else {
        generate_ssh_key()?;
        let pub_key = std::fs::read_to_string(&ssh_key_path)
            .map_err(|e| format!("Failed to read SSH public key: {}", e))?;
        std::fs::write(provision_dir.join("authorized_keys"), pub_key)
            .map_err(|e| format!("Failed to write authorized_keys: {}", e))?;
    }

    let binary_path = linux_agent_binary_path();
    if !binary_path.exists() {
        return Err(format!(
            "Linux agent binary not found at {}. Build failed?",
            binary_path.display()
        ));
    }
    std::fs::copy(&binary_path, provision_dir.join("rubberdux"))
        .map_err(|e| format!("Failed to copy binary to provision dir: {}", e))?;

    let share_arg = format!("__provision:{}", provision_dir.display());
    let mut vm_proc = std::process::Command::new("tart")
        .args(["run", &tmp_name, "--no-graphics", "--dir", &share_arg])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start VM: {}", e))?;

    println!("Waiting for VM network...");
    let ip = wait_for_ip(&tmp_name)?;
    println!("VM IP: {}", ip);

    println!("Waiting for SSH...");
    wait_for_ssh(&ip)?;

    println!("Running install script inside VM...");
    let remote_cmd = if image_name == "ubuntu24" {
        "sudo mkdir -p /mnt/shared && (sudo mount -t virtiofs com.apple.virtio-fs.automount /mnt/shared 2>/dev/null || true) && bash /mnt/shared/__provision/install.sh"
    } else {
        "bash '/Volumes/My Shared Files/__provision/install.sh'"
    };

    let install_result = Command::new("sshpass")
        .args([
            "-p",
            "admin",
            "ssh",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            &format!("admin@{}", ip),
            remote_cmd,
        ])
        .output()
        .map_err(|e| format!("SSH install failed: {}", e))?;

    if !install_result.status.success() {
        let stderr = String::from_utf8_lossy(&install_result.stderr);
        let stdout = String::from_utf8_lossy(&install_result.stdout);
        return Err(format!(
            "Install script failed:\nstdout:\n{}\nstderr:\n{}",
            stdout, stderr
        ));
    }

    println!("Install script completed.");

    let _ = Command::new("tart").args(["stop", &tmp_name]).status();

    let _ = vm_proc.wait();

    let _ = Command::new("tart")
        .args(["rename", &tmp_name, base_vm_name])
        .status();

    if let Ok(hash) = compute_provision_hash(base_vm_name) {
        let _ = write_stored_hash(base_vm_name, &hash);
    }

    println!("Base VM '{}' ready.", base_vm_name);
    Ok(())
}

fn wait_for_ip(vm_name: &str) -> Result<String, String> {
    for _ in 0..90 {
        let output = Command::new("tart").args(["ip", vm_name]).output();
        if let Ok(o) = output {
            let ip = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !ip.is_empty() && o.status.success() {
                return Ok(ip);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err(format!(
        "VM {} did not get an IP after 180 seconds",
        vm_name
    ))
}

fn wait_for_ssh(ip: &str) -> Result<(), String> {
    for _ in 0..60 {
        let result = Command::new("sshpass")
            .args([
                "-p",
                "admin",
                "ssh",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=5",
                &format!("admin@{}", ip),
                "true",
            ])
            .output();
        if let Ok(o) = result {
            if o.status.success() {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err("SSH not ready after 120 seconds".into())
}

fn generate_ssh_key() -> Result<(), String> {
    let key_path = dirs_home().join(".ssh").join("rubberdux_ed25519");
    let ssh_dir = key_path.parent().unwrap();
    std::fs::create_dir_all(ssh_dir).map_err(|e| format!("Failed to create ~/.ssh: {}", e))?;

    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &key_path.to_string_lossy(),
            "-N",
            "",
            "-q",
            "-C",
            "rubberdux-vm",
        ])
        .status()
        .map_err(|e| format!("ssh-keygen failed: {}", e))?;

    if !status.success() {
        return Err("ssh-keygen failed".into());
    }

    Ok(())
}

fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn total_memory_mb() -> usize {
    Command::new("sysctl")
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
