use std::path::{Path, PathBuf};
use std::process::Command;

fn project_root() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("Failed to get current directory: {}", e))
}

fn macos_app_dir(root: &PathBuf) -> PathBuf {
    root.join("apps").join("macos")
}

fn cargo_profile(release: bool) -> &'static str {
    if release { "release" } else { "debug" }
}

fn xcode_configuration(release: bool) -> &'static str {
    if release { "Release" } else { "Debug" }
}

fn build_rust_backend(release: bool) -> Result<(), String> {
    println!("Building Rust backend ({})...", cargo_profile(release));
    let mut args = vec!["build", "-p", "rubberdux"];
    if release {
        args.push("--release");
    }
    let status = Command::new("cargo")
        .args(&args)
        .status()
        .map_err(|e| format!("cargo build failed: {}", e))?;
    if !status.success() {
        return Err(format!("cargo build ({}) failed", cargo_profile(release)));
    }
    Ok(())
}

fn generate_xcode_project(app_dir: &PathBuf) -> Result<(), String> {
    let xcodegen = which("xcodegen")?;
    let status = Command::new(xcodegen)
        .current_dir(app_dir)
        .arg("generate")
        .status()
        .map_err(|e| format!("xcodegen failed: {}", e))?;
    if !status.success() {
        return Err("xcodegen generate failed".into());
    }
    Ok(())
}

fn build_xcode_project(app_dir: &PathBuf, release: bool) -> Result<PathBuf, String> {
    let config = xcode_configuration(release);
    let derived_data = derived_data_dir(app_dir);
    println!("Building macOS app ({})...", config);
    let status = Command::new("xcodebuild")
        .current_dir(app_dir)
        .args([
            "-project",
            "Rubberdux.xcodeproj",
            "-scheme",
            "Rubberdux",
            "-configuration",
            config,
        ])
        .arg("-derivedDataPath")
        .arg(&derived_data)
        .arg("build")
        .status()
        .map_err(|e| format!("xcodebuild failed: {}", e))?;
    if !status.success() {
        return Err(format!("xcodebuild ({}) failed", config));
    }

    let product = product_path(&derived_data, config);
    if !product.exists() {
        return Err(format!(
            "xcodebuild ({}) reported success but the product is missing at {}",
            config,
            product.display()
        ));
    }
    Ok(product)
}

/// The repo-local DerivedData directory, mirroring `apps/macos/scripts/build.sh`.
///
/// Pinning it makes the launched product the one this build just produced. The
/// previous global `~/Library/Developer/Xcode/DerivedData` scan returned the
/// first `Rubberdux-<hash>` directory that happened to hold a product, which
/// could be a stale app from a different checkout: Xcode keys that hash on the
/// project's absolute path, so moving or renaming the repo leaves orphan
/// products behind whose baked-in `RubberduxWorkspaceRoot` points at the old,
/// now-missing checkout.
fn derived_data_dir(app_dir: &Path) -> PathBuf {
    app_dir.join(".build").join("DerivedData")
}

/// The built `.app` path for a configuration. The bundle name follows
/// `PRODUCT_NAME` in `apps/macos/Config/<config>.xcconfig`, which is
/// `Rubberdux (<config>)` for the Debug and Release configurations the xtask
/// builds.
fn product_path(derived_data: &Path, config: &str) -> PathBuf {
    derived_data
        .join("Build")
        .join("Products")
        .join(config)
        .join(format!("Rubberdux ({}).app", config))
}

fn which(cmd: &str) -> Result<String, String> {
    let output = Command::new("which")
        .arg(cmd)
        .output()
        .map_err(|e| format!("Failed to locate {}: {}", cmd, e))?;
    if !output.status.success() {
        return Err(format!(
            "{} not found. Install with: brew install {}",
            cmd, cmd
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn build(release: bool) -> Result<(), String> {
    let root = project_root()?;
    let app_dir = macos_app_dir(&root);

    build_rust_backend(release)?;
    generate_xcode_project(&app_dir)?;
    build_xcode_project(&app_dir, release)?;

    println!("Build complete.");
    Ok(())
}

pub fn run(release: bool) -> Result<(), String> {
    let root = project_root()?;
    let app_dir = macos_app_dir(&root);

    build_rust_backend(release)?;
    generate_xcode_project(&app_dir)?;
    let app_path = build_xcode_project(&app_dir, release)?;

    println!("Launching {}...", app_path.display());
    Command::new("open")
        .arg(&app_path)
        .status()
        .map_err(|e| format!("Failed to open app: {}", e))?;

    Ok(())
}

/// Run the macOS app's XCTest target through the generated Xcode project.
/// Tests are orchestrated by cargo per the project convention (never bare
/// `xcodebuild`); this builds the Rust backend, regenerates the project, and
/// runs the `Rubberdux` scheme's test action (which hosts `RubberduxTests`).
pub fn test() -> Result<(), String> {
    let root = project_root()?;
    let app_dir = macos_app_dir(&root);

    build_rust_backend(false)?;
    generate_xcode_project(&app_dir)?;
    test_xcode_project(&app_dir)?;

    println!("Tests complete.");
    Ok(())
}

fn test_xcode_project(app_dir: &PathBuf) -> Result<(), String> {
    println!("Running macOS app tests (Debug)...");
    let status = Command::new("xcodebuild")
        .current_dir(app_dir)
        .args([
            "-project",
            "Rubberdux.xcodeproj",
            "-scheme",
            "Rubberdux",
            "-configuration",
            "Debug",
            "-destination",
            "platform=macOS",
        ])
        .arg("-derivedDataPath")
        .arg(derived_data_dir(app_dir))
        .arg("test")
        .status()
        .map_err(|e| format!("xcodebuild test failed: {}", e))?;
    if !status.success() {
        return Err("xcodebuild test failed".into());
    }
    Ok(())
}
