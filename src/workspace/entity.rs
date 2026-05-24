use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectStatus {
    Active,
    Paused,
    Completed,
}

impl Default for ProjectStatus {
    fn default() -> Self {
        Self::Active
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<NaiveDate>,
    #[serde(default)]
    pub status: ProjectStatus,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_manifest_with_deadline() {
        let m = ProjectManifest {
            name: "Blog Redesign".into(),
            description: "Redesign with Astro".into(),
            deadline: Some(NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()),
            status: ProjectStatus::Active,
            active: true,
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
            description: "Just exploring".into(),
            deadline: None,
            status: ProjectStatus::Active,
            active: true,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ProjectManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert!(!json.contains("deadline"));
    }

    #[test]
    fn test_project_status_default() {
        assert_eq!(ProjectStatus::default(), ProjectStatus::Active);
    }

    #[test]
    fn test_project_status_roundtrip() {
        let variants = vec![
            (ProjectStatus::Active, "active"),
            (ProjectStatus::Paused, "paused"),
            (ProjectStatus::Completed, "completed"),
        ];
        for (variant, expected_str) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{}\"", expected_str));
            let back: ProjectStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }
}
