use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use libtest_mimic::{Arguments, Failed, Trial};

const SCRIPT_EXTENSIONS: &[&str] = if cfg!(windows) {
    &["ps1", "bat", "cmd"]
} else {
    &["sh"]
};

fn host_triple() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => other,
    };

    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-macos"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        other => format!("{arch}-unknown-{other}"),
    }
}

fn discover_tests(e2e_dir: &Path) -> Vec<Trial> {
    let mut trials = Vec::new();
    let triple = host_triple();
    let triple_dir = e2e_dir.join(&triple);

    let Ok(entries) = std::fs::read_dir(&triple_dir) else {
        return trials;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        if !name_str.starts_with("test_") {
            continue;
        }

        let ext = Path::new(&*name_str)
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("");

        if !SCRIPT_EXTENSIONS.contains(&ext) {
            continue;
        }

        let test_name = name_str
            .trim_end_matches(&format!(".{ext}"))
            .to_string();
        let display_name = format!("{triple}/{test_name}");
        let script_path = entry.path();

        trials.push(Trial::test(display_name, move || {
            run_script(&script_path)
        }));
    }

    trials
}

fn run_script(script: &Path) -> Result<(), Failed> {
    let ext = script
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("");

    let output = match ext {
        "sh" => Command::new("bash").arg(script).output(),
        "ps1" => Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .output(),
        "bat" | "cmd" => Command::new("cmd").args(["/C"]).arg(script).output(),
        _ => return Err(format!("unsupported script extension: {ext}").into()),
    };

    let output = output.map_err(|e| format!("failed to run {}: {e}", script.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        Ok(())
    } else {
        Err(format!("--- stdout ---\n{stdout}--- stderr ---\n{stderr}").into())
    }
}

fn main() {
    let args = Arguments::from_args();
    let e2e_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/e2e");
    let tests = discover_tests(&e2e_dir);
    libtest_mimic::run(&args, tests).exit();
}
