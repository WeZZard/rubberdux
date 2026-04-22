use std::path::Path;

use crate::{lint, parser};

/// Discover all `*.testcase.md` files in `dir`, lint them, and parse them.
/// Panics if any file fails linting.
pub fn discover_cases(dir: &Path) -> Vec<crate::TestCase> {
    let mut cases = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            panic!("Failed to read cases directory {:?}: {}", dir, e);
        }
    };

    let mut lint_errors = Vec::new();

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            let stem = path.file_stem().unwrap().to_string_lossy();
            if stem.ends_with(".testcase") {
                let content = std::fs::read_to_string(&path).unwrap();

                // Lint first
                if let Err(errors) = lint(&content) {
                    lint_errors.push((path.display().to_string(), errors));
                    continue;
                }

                let name = stem.trim_end_matches(".testcase").to_string();
                match parser::parse(&content, &name) {
                    Ok(test_case) => cases.push(test_case),
                    Err(e) => panic!("Failed to parse {:?}: {}", path, e),
                }
            }
        }
    }

    if !lint_errors.is_empty() {
        eprintln!("\n=== Lint errors found ===");
        for (path, errors) in &lint_errors {
            eprintln!("\n✗ {}", path);
            for e in errors {
                eprintln!("  line {} [{}]: {}", e.line, e.rule, e.message);
            }
        }
        panic!("{} test case file(s) failed linting", lint_errors.len());
    }

    cases.sort_by(|a, b| a.name.cmp(&b.name));
    cases
}
