use std::path::PathBuf;
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
            "build",
        ])
        .status()
        .map_err(|e| format!("xcodebuild failed: {}", e))?;
    if !status.success() {
        return Err(format!("xcodebuild ({}) failed", config));
    }

    let derived_data = find_derived_data_product(config)?;
    Ok(derived_data)
}

fn find_derived_data_product(config: &str) -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("Cannot determine home directory")?;
    let derived = home.join("Library/Developer/Xcode/DerivedData");
    let entries = std::fs::read_dir(&derived)
        .map_err(|e| format!("Cannot read DerivedData: {}", e))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("Rubberdux-") {
            let product_name = if config == "Debug" {
                "Rubberdux (Debug).app"
            } else {
                "Rubberdux.app"
            };
            let app_path = entry
                .path()
                .join("Build/Products")
                .join(config)
                .join(product_name);
            if app_path.exists() {
                return Ok(app_path);
            }
        }
    }
    Err(format!(
        "Cannot find Rubberdux.app in DerivedData for configuration {}",
        config
    ))
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
