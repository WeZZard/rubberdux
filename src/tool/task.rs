use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::workspace::entity::{ProjectManifest, TaskManifest};
use crate::workspace::{format_yaml_front_matter, parse_yaml_front_matter, Workspace};

use super::ToolOutcome;

pub struct TaskTool {
    workspace: Arc<Workspace>,
}

impl TaskTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

impl super::Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("task.json")).unwrap()
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
                "list" => self.task_list(&args),
                "create" => self.task_create(&args),
                "retrieve" => self.task_retrieve(&args),
                "update" => self.task_update(&args),
                "complete" => self.task_complete(&args),
                _ => ToolOutcome::Immediate {
                    content: format!("Unknown action: {}", action),
                    is_error: true,
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Task helpers
// ---------------------------------------------------------------------------

impl TaskTool {
    fn find_task_dir(&self, name: &str) -> Option<std::path::PathBuf> {
        let tasks_dir = self.workspace.tasks_dir();
        let suffix = format!("-{}", name);
        let entries = match std::fs::read_dir(&tasks_dir) {
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

    fn read_task_manifest(
        &self,
        task_dir: &std::path::Path,
    ) -> Result<TaskManifest, String> {
        let index_path = task_dir.join("index.md");
        let content = std::fs::read_to_string(&index_path)
            .map_err(|e| format!("Failed to read {}: {}", index_path.display(), e))?;
        parse_yaml_front_matter::<TaskManifest>(&content)
            .map_err(|e| format!("Failed to parse task manifest: {}", e))
    }

    fn write_task_manifest(
        &self,
        task_dir: &std::path::Path,
        manifest: &TaskManifest,
    ) -> Result<(), String> {
        let index_path = task_dir.join("index.md");
        let content = format_yaml_front_matter(manifest, "")
            .map_err(|e| format!("Failed to format task manifest: {}", e))?;
        std::fs::write(&index_path, content)
            .map_err(|e| format!("Failed to write {}: {}", index_path.display(), e))
    }
}

// ---------------------------------------------------------------------------
// Project helpers (needed for bidirectional task-project links)
// ---------------------------------------------------------------------------

impl TaskTool {
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

impl TaskTool {
    fn task_list(&self, args: &serde_json::Value) -> ToolOutcome {
        let tasks_dir = self.workspace.tasks_dir();
        let entries = match std::fs::read_dir(&tasks_dir) {
            Ok(e) => e,
            Err(_) => {
                return ToolOutcome::Immediate {
                    content: "No tasks found.".into(),
                    is_error: false,
                };
            }
        };

        // Optional project filter
        let project_filter = args["fields"]
            .as_object()
            .and_then(|f| f.get("project"))
            .and_then(|v| v.as_str());

        let mut items: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            if let Ok(manifest) = self.read_task_manifest(&entry.path()) {
                // Apply project filter if provided
                if let Some(filter) = project_filter {
                    match &manifest.project {
                        Some(p) if p == filter => {}
                        _ => continue,
                    }
                }

                let dir_name = entry.file_name().to_string_lossy().to_string();
                items.push(format!(
                    "- {} [completed: {}]: {} (stop: {})",
                    dir_name, manifest.completed, manifest.description, manifest.stop_condition
                ));
            }
        }

        if items.is_empty() {
            return ToolOutcome::Immediate {
                content: "No tasks found.".into(),
                is_error: false,
            };
        }

        ToolOutcome::Immediate {
            content: items.join("\n"),
            is_error: false,
        }
    }

    fn task_create(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let fields = args["fields"].as_object();

        let stop_condition = match fields
            .and_then(|f| f.get("stop_condition"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s.to_string(),
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required field: stop_condition".into(),
                    is_error: true,
                };
            }
        };

        let description = fields
            .and_then(|f| f.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let project = fields
            .and_then(|f| f.get("project"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // If project is specified, verify it exists BEFORE creating anything
        if let Some(ref project_name) = project {
            if self.find_project_dir(project_name).is_none() {
                return ToolOutcome::Immediate {
                    content: format!("Project '{}' not found.", project_name),
                    is_error: true,
                };
            }
        }

        let timestamp = chrono::Utc::now().format("%Y-%m-%d-%H-%M-%S-UTC").to_string();
        let dir_name = format!("{}-{}", timestamp, name);
        let task_dir = self.workspace.tasks_dir().join(&dir_name);

        if task_dir.exists() {
            return ToolOutcome::Immediate {
                content: format!("Task directory '{}' already exists.", dir_name),
                is_error: true,
            };
        }

        // Create directory structure: artifacts/ and worktrees/
        for sub in &["artifacts", "worktrees"] {
            if let Err(e) = std::fs::create_dir_all(task_dir.join(sub)) {
                return ToolOutcome::Immediate {
                    content: format!("Failed to create {}/{}: {}", dir_name, sub, e),
                    is_error: true,
                };
            }
        }

        let manifest = TaskManifest {
            name: name.to_string(),
            description,
            stop_condition,
            project: project.clone(),
            completed: false,
        };

        if let Err(e) = self.write_task_manifest(&task_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        // If linked to a project, update the project's active_tasks
        if let Some(ref project_name) = project {
            let project_dir = self.find_project_dir(project_name).unwrap();
            match self.read_project_manifest(&project_dir) {
                Ok(mut project_manifest) => {
                    project_manifest.active_tasks.push(dir_name.clone());
                    if let Err(e) = self.write_project_manifest(&project_dir, &project_manifest) {
                        return ToolOutcome::Immediate {
                            content: format!(
                                "Task created but failed to update project '{}': {}",
                                project_name, e
                            ),
                            is_error: true,
                        };
                    }
                }
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!(
                            "Task created but failed to read project '{}': {}",
                            project_name, e
                        ),
                        is_error: true,
                    };
                }
            }
        }

        ToolOutcome::Immediate {
            content: format!("Task '{}' created at {}.", name, dir_name),
            is_error: false,
        }
    }

    fn task_retrieve(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let task_dir = match self.find_task_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Task '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        match self.read_task_manifest(&task_dir) {
            Ok(manifest) => {
                let project_str = manifest
                    .project
                    .as_deref()
                    .unwrap_or("none");
                ToolOutcome::Immediate {
                    content: format!(
                        "Name: {}\nDescription: {}\nStop condition: {}\nProject: {}\nCompleted: {}",
                        manifest.name,
                        manifest.description,
                        manifest.stop_condition,
                        project_str,
                        manifest.completed
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

    fn task_update(&self, args: &serde_json::Value) -> ToolOutcome {
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

        // Check for unknown fields — only "description" is mutable
        let valid_fields = ["description"];
        for key in fields.keys() {
            if !valid_fields.contains(&key.as_str()) {
                return ToolOutcome::Immediate {
                    content: format!(
                        "Unknown field '{}'. Valid fields for task update: {}",
                        key,
                        valid_fields.join(", ")
                    ),
                    is_error: true,
                };
            }
        }

        let task_dir = match self.find_task_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Task '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        let mut manifest = match self.read_task_manifest(&task_dir) {
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

        if let Err(e) = self.write_task_manifest(&task_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Task '{}' updated.", name),
            is_error: false,
        }
    }

    fn task_complete(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let task_dir = match self.find_task_dir(name) {
            Some(d) => d,
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Task '{}' not found.", name),
                    is_error: true,
                };
            }
        };

        // Read manifest and set completed = true
        let mut manifest = match self.read_task_manifest(&task_dir) {
            Ok(m) => m,
            Err(e) => {
                return ToolOutcome::Immediate {
                    content: e,
                    is_error: true,
                };
            }
        };

        manifest.completed = true;

        if let Err(e) = self.write_task_manifest(&task_dir, &manifest) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        // If task has a parent project, update the project manifest
        if let Some(ref project_name) = manifest.project {
            if let Some(project_dir) = self.find_project_dir(project_name) {
                if let Ok(mut project_manifest) = self.read_project_manifest(&project_dir) {
                    let dir_name = task_dir
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    project_manifest.active_tasks.retain(|t| t != &dir_name);
                    project_manifest.completed_tasks.push(dir_name);
                    let _ = self.write_project_manifest(&project_dir, &project_manifest);
                }
            }
        }

        // Move directory to archives/tasks/
        let dir_name = task_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let archive_dest = self
            .workspace
            .archives_dir()
            .join("tasks")
            .join(&dir_name);

        if let Err(e) = std::fs::rename(&task_dir, &archive_dest) {
            return ToolOutcome::Immediate {
                content: format!(
                    "Task '{}' marked complete but failed to move to archive: {}",
                    name, e
                ),
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!(
                "Task '{}' completed and archived to {}.",
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

    fn temp_tool() -> (std::path::PathBuf, TaskTool) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rubberdux-task-tool-test-{}-{}",
            ts, cnt
        ));
        let ws = Arc::new(Workspace { root: root.clone() });
        ws.ensure_dirs().unwrap();
        (root, TaskTool::new(ws))
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

    /// Helper: create a project directory with manifest for tests that need a parent project.
    fn create_test_project(tool: &TaskTool, project_name: &str) {
        let project_dir = tool
            .workspace
            .projects_dir()
            .join(format!("2026-01-01-{}", project_name));
        std::fs::create_dir_all(&project_dir).unwrap();
        let manifest = ProjectManifest {
            name: project_name.into(),
            description: "Test project".into(),
            deadline: None,
            completed: false,
            active_tasks: vec![],
            completed_tasks: vec![],
        };
        let content = format_yaml_front_matter(&manifest, "").unwrap();
        std::fs::write(project_dir.join("index.md"), content).unwrap();
    }

    #[test]
    fn test_task_list_empty() {
        let (_root, tool) = temp_tool();
        let args = serde_json::json!({"action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(!is_error);
        assert!(content.contains("No tasks"));
        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_create_and_list() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "research-astro",
            "fields": {
                "description": "Research Astro framework",
                "stop_condition": "summary document written"
            }
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error, "create failed: {}", content);

        // Verify task dir has artifacts/ and worktrees/ subdirs
        let task_dir = tool.find_task_dir("research-astro").unwrap();
        assert!(task_dir.join("artifacts").is_dir());
        assert!(task_dir.join("worktrees").is_dir());

        let list_args = serde_json::json!({"action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&list_args)));
        assert!(!is_error);
        assert!(content.contains("research-astro"));
        assert!(content.contains("Research Astro framework"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_create_requires_stop_condition() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "no-stop",
            "fields": {
                "description": "Missing stop condition"
            }
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(is_error);
        assert!(content.contains("stop_condition"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_create_with_project_link() {
        let (_root, tool) = temp_tool();

        // Create a project first
        create_test_project(&tool, "TestProj");

        let create_args = serde_json::json!({
            "action": "create",
            "name": "linked-task",
            "fields": {
                "stop_condition": "feature implemented",
                "project": "TestProj"
            }
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error, "create failed: {}", content);

        // Read back the project manifest and verify active_tasks contains the task dir name
        let project_dir = tool.find_project_dir("TestProj").unwrap();
        let project_manifest = tool.read_project_manifest(&project_dir).unwrap();
        assert_eq!(project_manifest.active_tasks.len(), 1);
        assert!(project_manifest.active_tasks[0].contains("linked-task"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_create_with_nonexistent_project() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "orphan-task",
            "fields": {
                "stop_condition": "done",
                "project": "NoExist"
            }
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(is_error);
        assert!(content.contains("not found"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_retrieve() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "retrievable",
            "fields": {
                "description": "Retrieve me",
                "stop_condition": "all tests pass"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "name": "retrievable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("retrievable"));
        assert!(content.contains("all tests pass"));
        assert!(content.contains("false")); // completed: false

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_update() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "updatable",
            "fields": {
                "description": "old desc",
                "stop_condition": "done"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "action": "update",
            "name": "updatable",
            "fields": {"description": "new desc"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "action": "retrieve",
            "name": "updatable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("new desc"));
        assert!(!content.contains("old desc"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_update_invalid_field() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "bad-field",
            "fields": {
                "description": "test",
                "stop_condition": "done"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "action": "update",
            "name": "bad-field",
            "fields": {"stop_condition": "new stop"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(is_error);
        assert!(content.contains("Unknown field"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_complete() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "completable",
            "fields": {
                "description": "Will be completed",
                "stop_condition": "done"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        // Verify task exists in tasks/
        let task_dir = tool.find_task_dir("completable").unwrap();
        let dir_name = task_dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(task_dir.exists());

        // Complete it
        let complete_args = serde_json::json!({
            "action": "complete",
            "name": "completable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&complete_args)));
        assert!(!is_error, "complete failed: {}", content);

        // Verify task dir no longer in tasks/
        assert!(tool.find_task_dir("completable").is_none());

        // Verify task dir exists in archives/tasks/
        let archive_dir = tool
            .workspace
            .archives_dir()
            .join("tasks")
            .join(&dir_name);
        assert!(archive_dir.exists());

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_complete_updates_project() {
        let (_root, tool) = temp_tool();

        // Create a project first
        create_test_project(&tool, "ParentProj");

        // Create a linked task
        let create_args = serde_json::json!({
            "action": "create",
            "name": "child-task",
            "fields": {
                "stop_condition": "implemented",
                "project": "ParentProj"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        // Capture the task dir name before completing
        let task_dir = tool.find_task_dir("child-task").unwrap();
        let task_dir_name = task_dir.file_name().unwrap().to_string_lossy().to_string();

        // Complete the task
        let complete_args = serde_json::json!({
            "action": "complete",
            "name": "child-task"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&complete_args)));
        assert!(!is_error, "complete failed: {}", content);

        // Verify task is in archives/tasks/
        let archive_dir = tool
            .workspace
            .archives_dir()
            .join("tasks")
            .join(&task_dir_name);
        assert!(archive_dir.exists());

        // Verify parent project's active_tasks no longer has it, completed_tasks has it
        let project_dir = tool.find_project_dir("ParentProj").unwrap();
        let project_manifest = tool.read_project_manifest(&project_dir).unwrap();
        assert!(
            !project_manifest.active_tasks.contains(&task_dir_name),
            "active_tasks should not contain the completed task"
        );
        assert!(
            project_manifest.completed_tasks.contains(&task_dir_name),
            "completed_tasks should contain the completed task"
        );

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_complete_nonexistent() {
        let (_root, tool) = temp_tool();

        let complete_args = serde_json::json!({
            "action": "complete",
            "name": "NoExist"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&complete_args)));
        assert!(is_error);
        assert!(content.contains("not found"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_unknown_action() {
        let (_root, tool) = temp_tool();

        let args = serde_json::json!({"action": "bogus"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(is_error);
        assert!(content.contains("Unknown action"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_task_directory_naming() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "action": "create",
            "name": "naming-check",
            "fields": {
                "stop_condition": "verified"
            }
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let task_dir = tool.find_task_dir("naming-check").unwrap();
        let dir_name = task_dir.file_name().unwrap().to_string_lossy().to_string();

        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}-\d{2}-\d{2}-\d{2}-UTC-.+$").unwrap();
        assert!(
            re.is_match(&dir_name),
            "Directory name '{}' does not match expected pattern",
            dir_name
        );

        let _ = std::fs::remove_dir_all(&_root);
    }
}
