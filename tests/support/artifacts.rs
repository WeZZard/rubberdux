#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RUN_CONTINUATION_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const RUN_ID_ENV: &str = "RUBBERDUX_TEST_RUN_ID";

#[derive(Clone, Debug)]
pub struct ArtifactRun {
    pub dir: PathBuf,
    pub run_id: String,
    pub timestamp: String,
}

static SUITE_RUNS: OnceLock<Mutex<HashMap<&'static str, ArtifactRun>>> = OnceLock::new();

pub fn integration_run() -> ArtifactRun {
    suite_run("integration")
}

pub fn system_run() -> ArtifactRun {
    suite_run("system")
}

pub fn integration_case_dir(test_name: &str) -> PathBuf {
    case_dir(integration_run(), test_name)
}

pub fn system_case_dir(test_name: &str) -> PathBuf {
    case_dir(system_run(), test_name)
}

fn suite_run(suite: &'static str) -> ArtifactRun {
    let runs = SUITE_RUNS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut runs = runs
        .lock()
        .expect("artifact run lock should not be poisoned");
    runs.entry(suite)
        .or_insert_with(|| create_suite_run(suite))
        .clone()
}

fn case_dir(run: ArtifactRun, test_name: &str) -> PathBuf {
    let dir = run.dir.join(test_name);
    fs::create_dir_all(&dir).expect("failed to create artifact dir");
    dir
}

fn create_suite_run(suite: &str) -> ArtifactRun {
    let clock = Clock::now();
    let timestamp = format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}-UTC",
        clock.year, clock.month, clock.day, clock.hours, clock.minutes, clock.seconds
    );
    let run_stem = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        clock.year, clock.month, clock.day, clock.hours, clock.minutes, clock.seconds
    );

    let results_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results");
    fs::create_dir_all(&results_dir).expect("failed to create results dir");

    let (run_dir, run_id) = configured_run(&results_dir).unwrap_or_else(|| {
        if suite == "system" {
            latest_run_without_suite(&results_dir, suite)
                .unwrap_or_else(|| create_unique_run_dir(&results_dir, &run_stem))
        } else {
            create_unique_run_dir(&results_dir, &run_stem)
        }
    });
    let dir = run_dir.join(suite);
    fs::create_dir_all(&dir).expect("failed to create suite results dir");

    ArtifactRun {
        dir,
        run_id,
        timestamp,
    }
}

fn configured_run(results_dir: &Path) -> Option<(PathBuf, String)> {
    let run_id = std::env::var(RUN_ID_ENV).ok()?;
    let dir = results_dir.join(&run_id);
    fs::create_dir_all(&dir).expect("failed to create configured results dir");
    Some((dir, run_id))
}

fn create_unique_run_dir(results_dir: &Path, run_stem: &str) -> (PathBuf, String) {
    for suffix in 0..100u8 {
        let run_id = if suffix == 0 {
            run_stem.to_string()
        } else {
            format!("{}_{}", run_stem, suffix)
        };
        let dir = results_dir.join(&run_id);

        match fs::create_dir(&dir) {
            Ok(()) => return (dir, run_id),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("failed to create results dir {}: {}", dir.display(), e),
        }
    }

    panic!(
        "failed to create unique results dir for {} under {}",
        run_stem,
        results_dir.display()
    );
}

fn latest_run_without_suite(results_dir: &Path, suite: &str) -> Option<(PathBuf, String)> {
    let mut runs = fs::read_dir(results_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let run_id = path.file_name()?.to_str()?.to_string();
            (path.is_dir() && is_run_id(&run_id) && !path.join(suite).exists())
                .then_some((path, run_id))
        })
        .filter(|(path, _)| is_recent(path))
        .collect::<Vec<_>>();

    runs.sort_by(|(_, left), (_, right)| left.cmp(right));
    runs.pop()
}

fn is_run_id(value: &str) -> bool {
    value.chars().all(|ch| ch.is_ascii_digit() || ch == '_')
}

fn is_recent(path: &Path) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed <= RUN_CONTINUATION_WINDOW)
}

struct Clock {
    year: u64,
    month: u64,
    day: u64,
    hours: u64,
    minutes: u64,
    seconds: u64,
}

impl Clock {
    fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let time_of_day = secs % 86400;
        let hours = time_of_day / 3600;
        let minutes = (time_of_day % 3600) / 60;
        let seconds = time_of_day % 60;
        let days = secs / 86400;
        let (year, month, day) = days_to_ymd(days);

        Self {
            year,
            month,
            day,
            hours,
            minutes,
            seconds,
        }
    }
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    let mut days = days_since_epoch;
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for days_in_month in month_days {
        if days < days_in_month {
            break;
        }
        days -= days_in_month;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn creates_unique_suite_run_dirs() {
        let root = temp_root("unique-suite-run-dirs");
        fs::create_dir_all(&root).unwrap();

        let (first_dir, first_id) = create_unique_run_dir(&root, "20260426_120000");
        let (second_dir, second_id) = create_unique_run_dir(&root, "20260426_120000");

        assert_eq!(first_id, "20260426_120000");
        assert_eq!(second_id, "20260426_120000_1");
        assert_eq!(first_dir.parent(), Some(root.as_path()));
        assert_eq!(second_dir.parent(), Some(root.as_path()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_latest_run_without_suite() {
        let root = temp_root("latest-run-without-suite");
        fs::create_dir_all(&root).unwrap();

        let old_layout_dir = root.join("20260426_115959-integration");
        let first_run = root.join("20260426_120000");
        let latest_run = root.join("20260426_120001");
        fs::create_dir_all(&old_layout_dir).unwrap();
        fs::create_dir_all(first_run.join("integration")).unwrap();
        fs::create_dir_all(latest_run.join("integration")).unwrap();

        let (_, run_id) = latest_run_without_suite(&root, "system").unwrap();
        assert_eq!(run_id, "20260426_120001");

        fs::create_dir_all(latest_run.join("system")).unwrap();
        let (_, run_id) = latest_run_without_suite(&root, "system").unwrap();
        assert_eq!(run_id, "20260426_120000");

        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rubberdux-artifact-run-test-{}-{}-{}-{}",
            std::process::id(),
            test_name,
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }
}
