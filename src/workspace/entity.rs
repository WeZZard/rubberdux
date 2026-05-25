use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_tasks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_tasks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn test_project_manifest_with_deadline() {
        let m = ProjectManifest {
            name: "Blog Redesign".into(),
            description: "Redesign with Astro".into(),
            deadline: Some(DateTime::<Utc>::from_naive_utc_and_offset(
                NaiveDate::from_ymd_opt(2026, 7, 15)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
                Utc,
            )),
            completed: false,
            active_tasks: vec![],
            completed_tasks: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(
            back.deadline,
            Some(DateTime::<Utc>::from_naive_utc_and_offset(
                NaiveDate::from_ymd_opt(2026, 7, 15)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
                Utc,
            ))
        );
    }

    #[test]
    fn test_project_manifest_without_deadline() {
        let m = ProjectManifest {
            name: "Experiment".into(),
            description: "Just exploring".into(),
            deadline: None,
            completed: false,
            active_tasks: vec![],
            completed_tasks: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert!(!json.contains("deadline"));
    }

    #[test]
    fn test_project_manifest_completed() {
        let m = ProjectManifest {
            name: "Done Project".into(),
            description: String::new(),
            deadline: None,
            completed: true,
            active_tasks: vec![],
            completed_tasks: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"completed\":true"));
    }

    #[test]
    fn test_project_manifest_with_tasks() {
        let m = ProjectManifest {
            name: "Task Project".into(),
            description: String::new(),
            deadline: None,
            completed: false,
            active_tasks: vec!["task-a".into()],
            completed_tasks: vec!["task-b".into()],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active_tasks, vec!["task-a".to_string()]);
        assert_eq!(back.completed_tasks, vec!["task-b".to_string()]);
    }

    #[test]
    fn test_project_manifest_default_completed() {
        let json = r#"{"name":"Minimal","description":""}"#;
        let m: ProjectManifest = serde_json::from_str(json).unwrap();
        assert!(!m.completed);
    }

    #[test]
    fn test_project_manifest_empty_tasks_not_serialized() {
        let m = ProjectManifest {
            name: "No Tasks".into(),
            description: String::new(),
            deadline: None,
            completed: false,
            active_tasks: vec![],
            completed_tasks: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("active_tasks"));
        assert!(!json.contains("completed_tasks"));
    }
}
