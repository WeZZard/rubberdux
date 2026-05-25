use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::workspace::entity::ProjectManifest;
use crate::workspace::{format_yaml_front_matter, parse_yaml_front_matter, Workspace};

use super::ToolOutcome;

pub struct ProjectTool {
    workspace: Arc<Workspace>,
}

impl ProjectTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

impl super::Tool for ProjectTool {
    fn name(&self) -> &str {
        "project"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("project.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to parse tool arguments: {}", e),
                        is_error: true,
                    };
                }
            };

            let action = match args["action"].as_str() {
                Some(a) => a,
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: action".into(),
                        is_error: true,
                    };
                }
            };

            match action {
                "list" => self.project_list(),
                "create" => self.project_create(&args),
                "retrieve" => self.project_retrieve(&args),
                "update" => self.project_update(&args),
                "complete" => self.project_complete(&args),
                _ => ToolOutcome::Immediate {
                    content: format!("Unknown action: {}", action),
                    is_error: true,
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl ProjectTool {
    fn find_project_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        let projects_dir = self.workspace.projects_dir();
        let suffix = format!("-{}", name);
        let entries = match std::fs::read_dir(&projects_dir) {
            Ok(e) => e,
            Err(_) => return None,
        };
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dir_name.ends_with(&suffix) && entry.path().is_dir() {
                return Some(entry.path());
            }
        }
        None
    }

    fn read_project_manifest(
        &self,
        project_dir: &std::path::Path,
    ) -> Result<ProjectManifest, String> {
        let index_path = project_dir.join("index.md");
        let content = std::fs::read_to_string(&index_path)
            .map_err(|e| format!("Failed to read {}: {}", index_path.display(), e))?;
        parse_yaml_front_matter::<ProjectManifest>(&content)
            .map_err(|e| format!("Failed to parse project manifest: {}", e))
    }

    fn write_project_manifest(
        &self,
        project_dir: &std::path::Path,
        manifest: &ProjectManifest,
    ) -> Result<(), String> {
        let index_path = project_dir.join("index.md");
        let content = format_yaml_front_matter(manifest, "")
            .map_err(|e| format!("Failed to format project manifest: {}", e))?;
        std::fs::write(&index_path, content)
            .map_err(|e| format!("Failed to write {}: {}", index_path.display(), e))
    }
}

// ---------------------------------------------------------------------------
// Action handlers
// ---------------------------------------------------------------------------

impl ProjectTool {
    fn project_list(&self) -> ToolOutcome {
        let projects_dir = self.workspace.projects_dir();
        let entries = match std::fs::read_dir(&projects_dir) {
            Ok(e) => e,
            Err(_) => {
                return ToolOutcome::Immediate {
                    content: "No projects found.".into(),
                    is_error: false,
                };
            }
        };

        let mut items: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(manifest) = self.read_project_manifest(&entry.path()) {
                let deadline_str = manifest
                    .deadline
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "none".into());
                items.push(format!(
                    "- {} [completed: {}, deadline: {}]: {}",
                    manifest.name, manifest.completed, deadline_str, manifest.description
                ));
            }
        }

        if items.is_empty() {
            return ToolOutcome::Immediate {
                content: "No projects found.".into(),
                is_error: false,
            };
        }

        ToolOutcome::Immediate {
            content: items.join("\n"),
            is_error: false,
        }
    }

    fn project_create(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let date_prefix = chrono::Local::now().format("%Y-%m-%d").to_string();
        let dir_name = format!("{}-{}", date_prefix, name);
        let project_dir = self.workspace.projects_dir().join(&dir_name);

        if project_dir.exists() {
            return ToolOutcome::Immediate {
                content: format!("Project directory '{}' already exists.", dir_name),
                is_error: true,
            };
        }

        let fields = args["fields"].as_object();
        let description = fields
            .and_then(|f| f.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let deadline = fields
            .and_then(|f| f.get("deadline"))
            .and_then(|v| v.as_str())
            .and_then(|s| {
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                        d.and_hms_opt(0, 0, 0).unwrap(),
                        chrono::Utc,
                    )
                })
            });

        let manifest = ProjectManifest {
            name: name.to_string(),
            description,
            deadline,
            completed: false,
            active_tasks: vec![],
            completed_tasks: vec![],
        };

        // Create directory structure: artifacts/ and worktrees/
        for sub in &["artifacts", "worktrees"] {
            if let Err(e) = std::fs::create_dir_all(project_dir.join(sub)) {
                return ToolOutcome::Immediate {
                    content: format!("Failed to create {}/{}: {}", dir_name, sub, e),
                    is_error: true,
                };
            }
        }

        if let Err(e) = self.write_project_manifest(&project_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Project '{}' created at {}.", name, dir_name),
            is_error: false,
        }
    }

    fn project_retrieve(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let project_dir = match self.find_project_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Project '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        match self.read_project_manifest(&project_dir) {
            Ok(manifest) => {
                let deadline_str = manifest
                    .deadline
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "none".into());
                ToolOutcome::Immediate {
                    content: format!(
                        "Name: {}\nDescription: {}\nDeadline: {}\nCompleted: {}",
                        manifest.name, manifest.description, deadline_str, manifest.completed
                    ),
                    is_error: false,
                }
            }
            Err(e) => ToolOutcome::Immediate {
                content: e,
                is_error: true,
            },
        }
    }

    fn project_update(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let fields = match args["fields"].as_object() {
            Some(f) => f,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: fields".into(),
                    is_error: true,
                };
            }
        };

        // Check for unknown fields
        let valid_fields = ["description", "deadline"];
        for key in fields.keys() {
            if !valid_fields.contains(&key.as_str()) {
                return ToolOutcome::Immediate {
                    content: format!(
                        "Unknown field '{}'. Valid fields for project: {}",
                        key,
                        valid_fields.join(", ")
                    ),
                    is_error: true,
                };
            }
        }

        let project_dir = match self.find_project_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Project '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        let mut manifest = match self.read_project_manifest(&project_dir) {
            Ok(m) => m,
            Err(e) => {
                return ToolOutcome::Immediate {
                    content: e,
                    is_error: true,
                };
            }
        };

        if let Some(desc) = fields.get("description").and_then(|v| v.as_str()) {
            manifest.description = desc.to_string();
        }

        if let Some(deadline_str) = fields.get("deadline").and_then(|v| v.as_str()) {
            match chrono::NaiveDate::parse_from_str(deadline_str, "%Y-%m-%d") {
                Ok(d) => {
                    manifest.deadline = Some(
                        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                            d.and_hms_opt(0, 0, 0).unwrap(),
                            chrono::Utc,
                        ),
                    );
                }
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!(
                            "Invalid deadline '{}': {}. Expected YYYY-MM-DD.",
                            deadline_str, e
                        ),
                        is_error: true,
                    };
                }
            }
        }

        if let Err(e) = self.write_project_manifest(&project_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Project '{}' updated.", name),
            is_error: false,
        }
    }

    fn project_complete(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let project_dir = match self.find_project_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Project '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        // Read manifest and set completed = true
        let mut manifest = match self.read_project_manifest(&project_dir) {
            Ok(m) => m,
            Err(e) => {
                return ToolOutcome::Immediate {
                    content: e,
                    is_error: true,
                };
            }
        };

        manifest.completed = true;

        if let Err(e) = self.write_project_manifest(&project_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        // Move directory to archives/projects/
        let dir_name = project_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let archive_dest = self
            .workspace
            .archives_dir()
            .join("projects")
            .join(&dir_name);

        if let Err(e) = std::fs::rename(&project_dir, &archive_dest) {
            return ToolOutcome::Immediate {
                content: format!(
                    "Project '{}' marked complete but failed to move to archive: {}",
                    name, e
                ),
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!(
                "Project '{}' completed and archived to {}.",
                name,
                archive_dest.display()
            ),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn temp_tool() -> (std::path::PathBuf, ProjectTool) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rubberdux-project-tool-test-{}-{}",
            ts, cnt
        ));
        let ws = Arc::new(Workspace { root: root.clone() });
        ws.ensure_dirs().unwrap();
        (root, ProjectTool::new(ws))
    }

    fn run<F: std::future::Future<Output = ToolOutcome>>(f: F) -> ToolOutcome {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    fn assert_immediate(outcome: ToolOutcome) -> (String, bool) {
        match outcome {
            ToolOutcome::Immediate { content, is_error } => (content, is_error),
            _ => panic!("Expected ToolOutcome::Immediate"),
        }
    }

    #[test]
    fn test_project_list_empty() {
        let (_root, tool) = temp_tool();
        let args = serde_json::json!({"action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(!is_error);
        assert!(content.contains("No projects found"));
        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_create_and_list() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "TestProj",
            "fields": {"description": "A test project"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error, "create failed: {}", content);

        // Verify artifacts/ and worktrees/ dirs exist
        let project_dir = tool.find_project_dir("TestProj").unwrap();
        assert!(project_dir.join("artifacts").is_dir());
        assert!(project_dir.join("worktrees").is_dir());

        let list_args = serde_json::json!({"action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&list_args)));
        assert!(!is_error);
        assert!(content.contains("TestProj"));
        assert!(content.contains("A test project"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_create_directory_structure() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "DirCheck",
            "fields": {"description": "Check dirs"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let project_dir = tool.find_project_dir("DirCheck").unwrap();
        assert!(project_dir.join("artifacts").is_dir(), "artifacts/ should exist");
        assert!(project_dir.join("worktrees").is_dir(), "worktrees/ should exist");
        assert!(
            !project_dir.join("scratches").exists(),
            "scratches/ should NOT exist"
        );

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_retrieve() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "Retrievable",
            "fields": {"description": "Retrieve me"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "name": "Retrievable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("Retrievable"));
        assert!(content.contains("false")); // completed: false

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "Updatable",
            "fields": {"description": "old desc"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "action": "update",
            "name": "Updatable",
            "fields": {"description": "new desc"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "name": "Updatable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("new desc"));
        assert!(!content.contains("old desc"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update_deadline() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "DeadlineProj"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "action": "update",
            "name": "DeadlineProj",
            "fields": {"deadline": "2027-06-15"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "name": "DeadlineProj"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("2027-06-15"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update_invalid_field() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "BadField"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "action": "update",
            "name": "BadField",
            "fields": {"nonexistent": "val"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(is_error);
        assert!(content.contains("Unknown field"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_complete() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "Completable",
            "fields": {"description": "Will be completed"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        // Verify project exists in projects/
        let project_dir = tool.find_project_dir("Completable").unwrap();
        let dir_name = project_dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(project_dir.exists());

        // Complete it
        let complete_args = serde_json::json!({
            "action": "complete",
            "name": "Completable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&complete_args)));
        assert!(!is_error, "complete failed: {}", content);

        // Verify project dir no longer in projects/
        assert!(tool.find_project_dir("Completable").is_none());

        // Verify project dir exists in archives/projects/
        let archive_dir = tool
            .workspace
            .archives_dir()
            .join("projects")
            .join(&dir_name);
        assert!(archive_dir.exists());

        // Verify manifest in archive has completed: true
        let index_content =
            std::fs::read_to_string(archive_dir.join("index.md")).unwrap();
        assert!(index_content.contains("completed: true"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_complete_nonexistent() {
        let (_root, tool) = temp_tool();

        let complete_args = serde_json::json!({
            "action": "complete",
            "name": "GhostProject"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&complete_args)));
        assert!(is_error);
        assert!(content.contains("not found"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_unknown_action() {
        let (_root, tool) = temp_tool();

        let args = serde_json::json!({"action": "destroy"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(is_error);
        assert!(content.contains("Unknown action"));

        let _ = std::fs::remove_dir_all(&_root);
    }
}
