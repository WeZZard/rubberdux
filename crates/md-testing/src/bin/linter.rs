fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dirs: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(|s| s.as_str()).collect()
    } else {
        vec!["tests/system/cases"]
    };

    let mut all_ok = true;
    let mut total_files = 0;

    for dir in &dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Warning: failed to read directory {}: {}", dir, e);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                let name = path.file_stem().unwrap().to_string_lossy();
                if !name.ends_with(".testcase") {
                    continue;
                }
                total_files += 1;
                let content = std::fs::read_to_string(&path).unwrap();
                match md_testing::lint(&content) {
                    Ok(()) => println!("✓ {}", path.display()),
                    Err(errors) => {
                        all_ok = false;
                        println!("✗ {}", path.display());
                        for e in errors {
                            println!("  line {} [{}]: {}", e.line, e.rule, e.message);
                        }
                    }
                }
            }
        }
    }

    println!("\nLinted {} files", total_files);

    if !all_ok {
        std::process::exit(1);
    }
}
