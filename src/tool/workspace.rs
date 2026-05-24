use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::workspace::entity::{ProjectManifest, ProjectStatus};
use crate::workspace::{format_yaml_front_matter, parse_yaml_front_matter, Workspace};

use super::ToolOutcome;

pub struct WorkspaceTool {
    workspace: Arc<Workspace>,
}

impl WorkspaceTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

impl super::Tool for WorkspaceTool {
    fn name(&self) -> &str {
        "workspace"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("workspace.json")).unwrap()
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

            let entity = match args["entity"].as_str() {
                Some(e) => e,
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: entity".into(),
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

            match entity {
                "project" => self.handle_project(action, &args),
                "artifact" | "resource" => ToolOutcome::Immediate {
                    content: format!("Entity '{}' is not yet implemented.", entity),
                    is_error: true,
                },
                _ => ToolOutcome::Immediate {
                    content: format!("Unknown entity: {}", entity),
                    is_error: true,
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Project handlers
// ---------------------------------------------------------------------------

impl WorkspaceTool {
    fn handle_project(&self, action: &str, args: &serde_json::Value) -> ToolOutcome {
        match action {
            "list" => self.project_list(),
            "create" => self.project_create(args),
            "retrieve" => self.project_retrieve(args),
            "update" => self.project_update(args),
            "deactivate" => self.project_deactivate(args),
            _ => ToolOutcome::Immediate {
                content: format!("Unknown action: {}", action),
                is_error: true,
            },
        }
    }

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
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "none".into());
                let active_str = if manifest.active { "active" } else { "inactive" };
                items.push(format!(
                    "- {} [status: {:?}, deadline: {}, {}]: {}",
                    manifest.name, manifest.status, deadline_str, active_str, manifest.description
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
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let status = fields
            .and_then(|f| f.get("status"))
            .and_then(|v| v.as_str())
            .map(|s| parse_project_status(s))
            .transpose();
        let status = match status {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                return ToolOutcome::Immediate {
                    content: e,
                    is_error: true,
                };
            }
        };

        let manifest = ProjectManifest {
            name: name.to_string(),
            description,
            deadline,
            status,
            active: true,
        };

        // Create directory structure
        for sub in &["scratches", "worktrees"] {
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
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "none".into());
                let active_str = if manifest.active { "active" } else { "inactive" };
                ToolOutcome::Immediate {
                    content: format!(
                        "Name: {}\nDescription: {}\nDeadline: {}\nStatus: {:?}\nActive: {}",
                        manifest.name, manifest.description, deadline_str, manifest.status,
                        active_str
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
        let valid_fields = ["description", "deadline", "status"];
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
                Ok(d) => manifest.deadline = Some(d),
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

        if let Some(status_str) = fields.get("status").and_then(|v| v.as_str()) {
            match parse_project_status(status_str) {
                Ok(s) => manifest.status = s,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: e,
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

    fn project_deactivate(&self, args: &serde_json::Value) -> ToolOutcome {
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

        let mut manifest = match self.read_project_manifest(&project_dir) {
            Ok(m) => m,
            Err(e) => {
                return ToolOutcome::Immediate {
                    content: e,
                    is_error: true,
                };
            }
        };

        manifest.active = false;

        if let Err(e) = self.write_project_manifest(&project_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Project '{}' deactivated.", name),
            is_error: false,
        }
    }
}

fn parse_project_status(s: &str) -> Result<ProjectStatus, String> {
    match s {
        "active" => Ok(ProjectStatus::Active),
        "paused" => Ok(ProjectStatus::Paused),
        "completed" => Ok(ProjectStatus::Completed),
        _ => Err(format!(
            "Unknown status '{}'. Valid values: active, paused, completed.",
            s
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn temp_tool() -> (std::path::PathBuf, WorkspaceTool) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rubberdux-workspace-tool-test-{}", ts));
        let ws = Arc::new(Workspace { root: root.clone() });
        ws.ensure_dirs().unwrap();
        (root, WorkspaceTool::new(ws))
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
        let args = serde_json::json!({"entity": "project", "action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(!is_error);
        assert!(content.contains("No projects found"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_create() {
        let (_root, tool) = temp_tool();

        let args = serde_json::json!({
            "entity": "project",
            "action": "create",
            "name": "TestProj",
            "fields": {"description": "A test project"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(!is_error, "create failed: {}", content);

        // Verify directory structure
        let project_dir = tool.find_project_dir("TestProj").unwrap();
        assert!(project_dir.exists());
        assert!(project_dir.join("index.md").exists());
        assert!(project_dir.join("scratches").is_dir());
        assert!(project_dir.join("worktrees").is_dir());

        // Verify the dir name has a date prefix
        let dir_name = project_dir.file_name().unwrap().to_string_lossy();
        assert!(dir_name.ends_with("-TestProj"));
        // Check date prefix format: YYYY-MM-DD
        let date_part = &dir_name[..10];
        assert!(
            chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d").is_ok(),
            "Directory should have date prefix, got: {}",
            date_part
        );

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_create_and_list() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "project",
            "action": "create",
            "name": "Listed",
            "fields": {"description": "Show me", "status": "active"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let list_args = serde_json::json!({"entity": "project", "action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&list_args)));
        assert!(!is_error);
        assert!(content.contains("Listed"));
        assert!(content.contains("Active"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update_single_field() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "project",
            "action": "create",
            "name": "SingleUpdate"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "entity": "project",
            "action": "update",
            "name": "SingleUpdate",
            "fields": {"status": "paused"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "entity": "project",
            "action": "retrieve",
            "name": "SingleUpdate"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("Paused"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update_multiple_fields() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "project",
            "action": "create",
            "name": "MultiUpdate"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "entity": "project",
            "action": "update",
            "name": "MultiUpdate",
            "fields": {"deadline": "2027-01-01", "description": "Updated"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "entity": "project",
            "action": "retrieve",
            "name": "MultiUpdate"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("2027-01-01"));
        assert!(content.contains("Updated"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_project_update_invalid_field() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "project",
            "action": "create",
            "name": "BadField"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "entity": "project",
            "action": "update",
            "name": "BadField",
            "fields": {"nonexistent": "val"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(is_error);
        assert!(content.contains("Unknown field"));
        assert!(content.contains("description"));
        assert!(content.contains("deadline"));
        assert!(content.contains("status"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_unimplemented_entity() {
        let (_root, tool) = temp_tool();
        let args = serde_json::json!({"entity": "artifact", "action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(is_error);
        assert!(content.contains("not yet implemented"));

        let _ = std::fs::remove_dir_all(&_root);
    }
}
