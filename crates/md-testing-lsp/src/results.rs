use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use md_testing::TestResults;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;

/// Stores and manages test results from the results directory.
#[derive(Debug)]
pub struct ResultsStore {
    /// Map of testcase name -> latest results
    results: RwLock<HashMap<String, TestResults>>,
    /// Path to the results directory
    results_dir: RwLock<Option<PathBuf>>,
}

impl ResultsStore {
    pub fn new() -> Self {
        Self {
            results: RwLock::new(HashMap::new()),
            results_dir: RwLock::new(None),
        }
    }

    /// Initialize the results directory from a workspace root.
    pub async fn init(&self, workspace_root: &Path) {
        let results_dir = workspace_root.join("tests").join("results");
        *self.results_dir.write().await = Some(results_dir.clone());
        self.scan_results_dir(&results_dir).await;
    }

    /// Watch the results directory for changes.
    pub async fn watch_results_dir(
        self: Arc<Self>,
        client: Client,
        documents: Arc<RwLock<HashMap<Url, String>>>,
    ) {
        // For now, poll every 5 seconds
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;

            let results_dir = self.results_dir.read().await.clone();
            if let Some(dir) = results_dir {
                let changed = self.scan_results_dir(&dir).await;
                if changed {
                    // Re-publish diagnostics for all open testcase files
                    let docs = documents.read().await;
                    for (uri, content) in docs.iter() {
                        if uri.path().ends_with(".testcase.md") {
                            let diagnostics = crate::diagnostics::build_diagnostics(
                                content,
                                uri,
                                &self,
                            ).await;
                            client.publish_diagnostics(uri.clone(), diagnostics, None).await;
                        }
                    }
                }
            }
        }
    }

    /// Scan the results directory and load the latest results for each test case.
    /// Returns true if any results were updated.
    async fn scan_results_dir(&self,
        results_dir: &Path,
    ) -> bool {
        if !results_dir.exists() {
            return false;
        }

        let mut changed = false;
        let mut results = self.results.write().await;

        // Find all run directories (sorted by name, which is timestamp-based)
        let mut run_dirs: Vec<PathBuf> = match std::fs::read_dir(results_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect(),
            Err(_) => return false,
        };

        run_dirs.sort();

        // For each run directory, load results.json files
        for run_dir in run_dirs {
            let _run_id = run_dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            let entries = match std::fs::read_dir(&run_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let case_dir = entry.path();
                if !case_dir.is_dir() {
                    continue;
                }

                let results_json = case_dir.join("results.json");
                if !results_json.exists() {
                    continue;
                }

                match TestResults::read(&results_json) {
                    Ok(test_results) => {
                        let case_name = test_results.testcase_name.clone();
                        // Only update if this is newer than what we have
                        let should_update = results
                            .get(&case_name)
                            .map(|existing| test_results.run_id > existing.run_id)
                            .unwrap_or(true);
                        if should_update {
                            results.insert(case_name, test_results);
                            changed = true;
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to read results.json at {:?}: {}", results_json, e);
                    }
                }
            }
        }

        changed
    }

    /// Get the latest results for a test case by name.
    pub async fn get_results(&self,
        case_name: &str,
    ) -> Option<TestResults> {
        self.results.read().await.get(case_name).cloned()
    }
}
