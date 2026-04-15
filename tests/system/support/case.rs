use std::path::Path;

/// A parsed end-to-end test case.
pub struct TestCase {
    pub name: String,
    pub user_message: String,
    pub assertions: Vec<String>,
}

/// Discover all `testcase_*.md` files in `dir` and parse them.
pub fn discover_cases(dir: &Path) -> Vec<TestCase> {
    let mut cases = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            panic!("Failed to read cases directory {:?}: {}", dir, e);
        }
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            let stem = path.file_stem().unwrap().to_string_lossy();
            if stem.starts_with("testcase_") {
                cases.push(parse_case(&path));
            }
        }
    }
    cases.sort_by(|a, b| a.name.cmp(&b.name));
    cases
}

fn parse_case(path: &Path) -> TestCase {
    let content = std::fs::read_to_string(path).unwrap();
    let name = path.file_stem().unwrap().to_string_lossy().to_string();

    let user_heading = "## User Message";
    let assertions_heading = "## Assertions";

    let user_start = content.find(user_heading).map(|i| i + user_heading.len());
    let assertions_pos = content.find(assertions_heading);
    let assertions_content_start = assertions_pos.map(|i| i + assertions_heading.len());

    let user_message = match (user_start, assertions_pos) {
        (Some(us), Some(ap)) => content[us..ap].trim().to_string(),
        (Some(us), None) => content[us..].trim().to_string(),
        _ => String::new(),
    };

    let assertions = if let Some(as_) = assertions_content_start {
        content[as_..]
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.starts_with("- ") {
                    Some(trimmed[2..].trim().to_string())
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    TestCase {
        name,
        user_message,
        assertions,
    }
}
