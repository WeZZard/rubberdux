use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Responsibility {
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsibilitiesDoc {
    pub items: Vec<Responsibility>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<NaiveDate>,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_responsibility_serde_roundtrip() {
        let r = Responsibility {
            title: "Maintain blog".into(),
            description: "Keep wezzard.com updated".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Responsibility = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn test_responsibilities_doc_serde_roundtrip() {
        let doc = ResponsibilitiesDoc {
            items: vec![
                Responsibility {
                    title: "Maintain blog".into(),
                    description: "Keep wezzard.com updated".into(),
                },
                Responsibility {
                    title: "Manage VPS".into(),
                    description: "Monitor fleet health".into(),
                },
            ],
        };
        let json = serde_json::to_string(&doc).unwrap();
        let back: ResponsibilitiesDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
        assert_eq!(back.items.len(), 2);
    }

    #[test]
    fn test_project_manifest_with_deadline() {
        let m = ProjectManifest {
            name: "Blog Redesign".into(),
            deadline: Some(NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()),
            description: "Redesign with Astro".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.deadline, Some(NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()));
    }

    #[test]
    fn test_project_manifest_without_deadline() {
        let m = ProjectManifest {
            name: "Experiment".into(),
            deadline: None,
            description: "Just exploring".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert!(!json.contains("deadline"));
    }
}
