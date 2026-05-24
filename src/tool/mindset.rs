use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::frontmatter::{format_yaml_front_matter, parse_yaml_front_matter};
use crate::mindset::entity::{Responsibility, ResponsibilitiesDoc};
use crate::mindset::Mindset;
use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct MindsetTool {
    mindset: Arc<Mindset>,
}

impl MindsetTool {
    pub fn new(mindset: Arc<Mindset>) -> Self {
        Self { mindset }
    }
}

impl super::Tool for MindsetTool {
    fn name(&self) -> &str {
        "mindset"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("mindset.json")).unwrap()
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
                "responsibility" => self.handle_responsibility(action, &args),
                _ => ToolOutcome::Immediate {
                    content: format!("Unknown entity: {}", entity),
                    is_error: true,
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Responsibility handlers
// ---------------------------------------------------------------------------

impl MindsetTool {
    fn handle_responsibility(
        &self,
        action: &str,
        args: &serde_json::Value,
    ) -> ToolOutcome {
        match action {
            "list" => self.responsibility_list(),
            "create" => self.responsibility_create(args),
            "retrieve" => self.responsibility_retrieve(args),
            "update" => self.responsibility_update(args),
            "deactivate" => self.responsibility_deactivate(args),
            _ => ToolOutcome::Immediate {
                content: format!("Unknown action: {}", action),
                is_error: true,
            },
        }
    }

    fn read_responsibilities_doc(&self) -> ResponsibilitiesDoc {
        let path = self.mindset.responsibilities_path();
        if !path.exists() {
            return ResponsibilitiesDoc { items: vec![] };
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => parse_yaml_front_matter::<ResponsibilitiesDoc>(&content)
                .unwrap_or(ResponsibilitiesDoc { items: vec![] }),
            Err(_) => ResponsibilitiesDoc { items: vec![] },
        }
    }

    fn write_responsibilities_doc(&self, doc: &ResponsibilitiesDoc) -> Result<(), String> {
        let path = self.mindset.responsibilities_path();
        let content = format_yaml_front_matter(doc, "")
            .map_err(|e| format!("Failed to format responsibilities: {}", e))?;
        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))
    }

    fn responsibility_list(&self) -> ToolOutcome {
        let doc = self.read_responsibilities_doc();
        if doc.items.is_empty() {
            return ToolOutcome::Immediate {
                content: "No responsibilities found.".into(),
                is_error: false,
            };
        }
        let lines: Vec<String> = doc
            .items
            .iter()
            .map(|r| {
                let status = if r.active { "active" } else { "inactive" };
                format!("- {} [{}]: {}", r.title, status, r.description)
            })
            .collect();
        ToolOutcome::Immediate {
            content: lines.join("\n"),
            is_error: false,
        }
    }

    fn responsibility_create(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };
        let description = args["fields"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let mut doc = self.read_responsibilities_doc();
        doc.items.push(Responsibility {
            title: name.to_string(),
            description,
            active: true,
        });

        if let Err(e) = self.write_responsibilities_doc(&doc) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Responsibility '{}' created.", name),
            is_error: false,
        }
    }

    fn responsibility_retrieve(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let doc = self.read_responsibilities_doc();
        match doc.items.iter().find(|r| r.title == name) {
            Some(r) => {
                let status = if r.active { "active" } else { "inactive" };
                ToolOutcome::Immediate {
                    content: format!(
                        "Title: {}\nDescription: {}\nStatus: {}",
                        r.title, r.description, status
                    ),
                    is_error: false,
                }
            }
            None => ToolOutcome::Immediate {
                content: format!("Responsibility '{}' not found.", name),
                is_error: true,
            },
        }
    }

    fn responsibility_update(&self, args: &serde_json::Value) -> ToolOutcome {
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
        let valid_fields = ["description"];
        for key in fields.keys() {
            if !valid_fields.contains(&key.as_str()) {
                return ToolOutcome::Immediate {
                    content: format!(
                        "Unknown field '{}'. Valid fields for responsibility: {}",
                        key,
                        valid_fields.join(", ")
                    ),
                    is_error: true,
                };
            }
        }

        let mut doc = self.read_responsibilities_doc();
        match doc.items.iter_mut().find(|r| r.title == name) {
            Some(r) => {
                if let Some(desc) = fields.get("description").and_then(|v| v.as_str()) {
                    r.description = desc.to_string();
                }
            }
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Responsibility '{}' not found.", name),
                    is_error: true,
                };
            }
        }

        if let Err(e) = self.write_responsibilities_doc(&doc) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Responsibility '{}' updated.", name),
            is_error: false,
        }
    }

    fn responsibility_deactivate(&self, args: &serde_json::Value) -> ToolOutcome {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => {
                return ToolOutcome::Immediate {
                    content: "Missing required parameter: name".into(),
                    is_error: true,
                };
            }
        };

        let mut doc = self.read_responsibilities_doc();
        match doc.items.iter_mut().find(|r| r.title == name) {
            Some(r) => {
                r.active = false;
            }
            None => {
                return ToolOutcome::Immediate {
                    content: format!("Responsibility '{}' not found.", name),
                    is_error: true,
                };
            }
        }

        if let Err(e) = self.write_responsibilities_doc(&doc) {
            return ToolOutcome::Immediate {
                content: e,
                is_error: true,
            };
        }

        ToolOutcome::Immediate {
            content: format!("Responsibility '{}' deactivated.", name),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn temp_tool() -> (std::path::PathBuf, MindsetTool) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("rubberdux-mindset-tool-test-{}", ts));
        let ms = Arc::new(Mindset { root: root.clone() });
        ms.ensure_dirs().unwrap();
        (root, MindsetTool::new(ms))
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
    fn test_responsibility_list_empty() {
        let (_root, tool) = temp_tool();
        let args = serde_json::json!({"entity": "responsibility", "action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(!is_error);
        assert!(content.contains("No responsibilities found"));
        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_responsibility_create_and_list() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "responsibility",
            "action": "create",
            "name": "Test",
            "fields": {"description": "A test responsibility"}
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error, "create failed: {}", content);

        let list_args =
            serde_json::json!({"entity": "responsibility", "action": "list"}).to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&list_args)));
        assert!(!is_error);
        assert!(content.contains("Test"));
        assert!(content.contains("A test responsibility"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_responsibility_retrieve() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "responsibility",
            "action": "create",
            "name": "Retrieve Me",
            "fields": {"description": "Retrievable"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "entity": "responsibility",
            "action": "retrieve",
            "name": "Retrieve Me"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("Retrieve Me"));
        assert!(content.contains("Retrievable"));
        assert!(content.contains("active"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_responsibility_update() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "responsibility",
            "action": "create",
            "name": "Updatable",
            "fields": {"description": "old"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let update_args = serde_json::json!({
            "entity": "responsibility",
            "action": "update",
            "name": "Updatable",
            "fields": {"description": "new"}
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&update_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "entity": "responsibility",
            "action": "retrieve",
            "name": "Updatable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("new"));
        assert!(!content.contains("old"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_responsibility_deactivate() {
        let (_root, tool) = temp_tool();

        let create_args = serde_json::json!({
            "entity": "responsibility",
            "action": "create",
            "name": "Deactivatable"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&create_args)));
        assert!(!is_error);

        let deactivate_args = serde_json::json!({
            "entity": "responsibility",
            "action": "deactivate",
            "name": "Deactivatable"
        })
        .to_string();
        let (_, is_error) = assert_immediate(run(tool.execute(&deactivate_args)));
        assert!(!is_error);

        let retrieve_args = serde_json::json!({
            "entity": "responsibility",
            "action": "retrieve",
            "name": "Deactivatable"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&retrieve_args)));
        assert!(!is_error);
        assert!(content.contains("inactive"));

        let _ = std::fs::remove_dir_all(&_root);
    }

    #[test]
    fn test_responsibility_retrieve_nonexistent() {
        let (_root, tool) = temp_tool();
        let args = serde_json::json!({
            "entity": "responsibility",
            "action": "retrieve",
            "name": "NoExist"
        })
        .to_string();
        let (content, is_error) = assert_immediate(run(tool.execute(&args)));
        assert!(is_error);
        assert!(content.contains("not found"));

        let _ = std::fs::remove_dir_all(&_root);
    }
}
