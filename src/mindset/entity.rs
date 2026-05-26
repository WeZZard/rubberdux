use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Responsibility {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_responsibility_serde_roundtrip() {
        let r = Responsibility {
            title: "Maintain blog".into(),
            description: "Keep wezzard.com updated".into(),
            active: true,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Responsibility = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn test_responsibility_deactivated_roundtrip() {
        let r = Responsibility {
            title: "Old task".into(),
            description: "No longer needed".into(),
            active: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Responsibility = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(json.contains("active"));
    }

    #[test]
    fn test_responsibility_deserialize_without_active() {
        let json = r#"{"title":"T","description":"D"}"#;
        let r: Responsibility = serde_json::from_str(json).unwrap();
        assert_eq!(r.active, true);
    }
}
