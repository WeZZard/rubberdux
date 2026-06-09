//! `cargo xtask lint`: the design-documentation structure linter.
//!
//! Contract: returns `Ok(())` when the design-documentation structure is
//! well-maintained, and `Err` (printing each violation) otherwise, so the
//! caller can exit non-zero. It enforces the discovery convention recorded in
//! the root `CLAUDE.md` "Design Documentation" section, so that the path-mirror
//! mapping stays trustworthy without hand-maintained pointers.
//!
//! Three checks, matching that section:
//!   1. No orphan docs — every `docs/**/*.md` maps to an existing code path.
//!   2. Valid mirrored location — the doc's directory mirrors a real code dir.
//!   3. Comment-pointer resolution — every `docs/….md` referenced from source
//!      resolves to a file that exists.
//!
//! Checks 1 and 2 are the same mapping viewed two ways, so a single
//! "the mirrored code directory must exist" test satisfies both.

use std::fs;
use std::path::{Path, PathBuf};

/// Directory names that never contain source or design docs worth scanning.
const SKIP_DIRS: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    ".build",
    "DerivedData",
    "results", // tests/results: large test-output tree, not source
];

/// Source file extensions scanned for `docs/….md` pointers.
const SOURCE_EXTS: &[&str] = &["rs", "swift", "m", "mm", "h", "hpp", "ts", "tsx", "js", "jsx"];

/// Entry point for the `lint` subcommand.
pub fn lint() -> Result<(), String> {
    let root = repo_root()?;
    let mut violations: Vec<String> = Vec::new();

    check_docs_mirror(&root, &mut violations)?;
    check_comment_pointers(&root, &mut violations)?;

    if violations.is_empty() {
        println!("design-doc lint: ok");
        Ok(())
    } else {
        eprintln!("design-doc lint: {} violation(s)", violations.len());
        for v in &violations {
            eprintln!("  - {v}");
        }
        Err(format!("{} design-documentation violation(s)", violations.len()))
    }
}

/// Locate the repository root by walking up from the working directory until a
/// `.git` entry is found. `cargo xtask` runs from the invocation directory, so
/// this keeps the linter correct regardless of where it is launched.
fn repo_root() -> Result<PathBuf, String> {
    let mut dir = std::env::current_dir().map_err(|e| format!("current_dir: {e}"))?;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("not inside a git repository (no .git found)".into());
        }
    }
}

/// Check 1 + 2: every design document under `docs/` mirrors an existing code
/// directory. A doc at `docs/<rel>/file.md` is valid when its directory `<rel>`
/// resolves to an existing directory either at the repo root (`apps/`, `xtask/`,
/// `crates/`, …) or under `src/`. A doc directly under `docs/` is cross-cutting
/// and mirrors the repo root.
fn check_docs_mirror(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let docs = root.join("docs");
    if !docs.is_dir() {
        return Ok(());
    }

    let mut files = Vec::new();
    collect_files(&docs, &mut files)?;

    for file in files {
        if file.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let rel = file
            .strip_prefix(&docs)
            .map_err(|e| format!("strip_prefix: {e}"))?;
        let rel_dir = rel.parent().unwrap_or_else(|| Path::new(""));

        let ok = rel_dir.as_os_str().is_empty()
            || root.join(rel_dir).is_dir()
            || root.join("src").join(rel_dir).is_dir();

        if !ok {
            violations.push(format!(
                "orphan design doc: `docs/{}` mirrors no code directory (expected `{}` or `src/{}`)",
                rel.display(),
                rel_dir.display(),
                rel_dir.display(),
            ));
        }
    }
    Ok(())
}

/// Check 3: every `docs/….md` reference inside a source file resolves to an
/// existing document, so the code-to-spec pointers in comments stay honest.
fn check_comment_pointers(root: &Path, violations: &mut Vec<String>) -> Result<(), String> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;

    for file in files {
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !SOURCE_EXTS.contains(&ext) {
            continue;
        }
        // Skip non-UTF-8 / unreadable files silently — they carry no pointers.
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            for token in doc_pointers(line) {
                if !root.join(&token).exists() {
                    let rel = file.strip_prefix(root).unwrap_or(&file);
                    violations.push(format!(
                        "broken doc pointer in {}:{}: `{}`",
                        rel.display(),
                        idx + 1,
                        token,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Extract every `docs/….md` path token from a single line. A token starts at
/// `docs/` and runs while characters are valid for a path; only tokens ending
/// in `.md` are treated as design-document pointers.
fn doc_pointers(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(offset) = line[search_from..].find("docs/") {
        let start = search_from + offset;
        let mut end = start;
        for (j, ch) in line[start..].char_indices() {
            if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-') {
                end = start + j + ch.len_utf8();
            } else {
                break;
            }
        }
        let token = &line[start..end];
        if token.ends_with(".md") {
            out.push(token.to_string());
        }
        search_from = end.max(start + "docs/".len());
    }
    out
}

/// Recursively collect files under `dir`, skipping the directories in
/// `SKIP_DIRS` at any depth.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry in {}: {e}", dir.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("file_type: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_files(&entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}
