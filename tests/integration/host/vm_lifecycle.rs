use std::path::PathBuf;

use rubberdux::host::HostConfig;
use rubberdux::vm::manager::VMManager;

/// Test that HostConfig::from_env parses correctly.
#[test]
fn test_host_config_from_env() {
    unsafe {
        std::env::set_var("RUBBERDUX_VM_IMAGE", "test-image");
        std::env::set_var("RUBBERDUX_VM_SHARES", "/tmp/test-shares");
        std::env::set_var("RUBBERDUX_RPC_PORT", "12345");
        std::env::set_var("RUBBERDUX_HOST_IP", "192.168.1.1");
    }

    let config = HostConfig::from_env();

    assert_eq!(config.vm_image, "test-image");
    assert_eq!(config.share_root, PathBuf::from("/tmp/test-shares"));
    assert_eq!(config.rpc_port, 12345);
    assert_eq!(config.host_ip, "192.168.1.1");

    // Clean up
    unsafe {
        std::env::remove_var("RUBBERDUX_VM_IMAGE");
        std::env::remove_var("RUBBERDUX_VM_SHARES");
        std::env::remove_var("RUBBERDUX_RPC_PORT");
        std::env::remove_var("RUBBERDUX_HOST_IP");
    }
}

/// Test that HostConfig uses defaults when env vars are missing.
#[test]
fn test_host_config_defaults() {
    // Remove env vars to test defaults
    unsafe {
        std::env::remove_var("RUBBERDUX_VM_IMAGE");
        std::env::remove_var("RUBBERDUX_VM_SHARES");
        std::env::remove_var("RUBBERDUX_RPC_PORT");
        std::env::remove_var("RUBBERDUX_HOST_IP");
    }

    let config = HostConfig::from_env();

    assert_eq!(config.vm_image, "rubberdux-base-ubuntu24-release");
    assert_eq!(config.share_root, PathBuf::from("./vm-shares"));
    assert_eq!(config.rpc_port, 19384);
    assert_eq!(config.host_ip, "192.168.64.1");
}

/// Test that VMManager::share_dir returns correct paths.
#[test]
fn test_vm_manager_share_dir() {
    let mgr = VMManager::new("img".into(), PathBuf::from("/tmp/shares"));
    assert_eq!(mgr.share_dir("main"), PathBuf::from("/tmp/shares/main"));
    assert_eq!(mgr.share_dir("task-1"), PathBuf::from("/tmp/shares/task-1"));
}

/// Test that VMManager::new creates a clean instance.
#[test]
fn test_vm_manager_new() {
    let mgr = VMManager::new("base-image".into(), PathBuf::from("/tmp/test"));
    assert_eq!(mgr.default_image(), "base-image");
}

/// Test that VMManager can be configured with memory and CPU.
#[test]
fn test_vm_manager_with_resources() {
    let mgr = VMManager::new("img".into(), PathBuf::from("/tmp/test"))
        .with_memory_mb(8192)
        .with_cpu_count(6);

    // The fields are private, but we can verify the builder pattern works
    // by checking it doesn't panic
    let _ = mgr.share_dir("test");
}
