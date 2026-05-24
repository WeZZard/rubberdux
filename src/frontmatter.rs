use crate::error::Error;

pub fn parse_yaml_front_matter<T: serde::de::DeserializeOwned>(content: &str) -> Result<T, Error> {
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return Err(Error::FrontMatter("Invalid front matter: missing --- delimiters".into()));
    }
    let yaml = parts[1].trim();
    serde_yaml::from_str(yaml)
        .map_err(|e| Error::FrontMatter(format!("Failed to parse YAML front matter: {}", e)))
}

pub fn format_yaml_front_matter<T: serde::Serialize>(data: &T, body: &str) -> Result<String, Error> {
    let yaml = serde_yaml::to_string(data)
        .map_err(|e| Error::FrontMatter(format!("Failed to serialize YAML: {}", e)))?;
    Ok(format!("---\n{}---\n{}", yaml, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestDoc {
        title: String,
        count: u32,
    }

    #[test]
    fn test_parse_valid() {
        let content = "---\ntitle: Hello\ncount: 42\n---\nbody text";
        let doc: TestDoc = parse_yaml_front_matter(content).unwrap();
        assert_eq!(doc.title, "Hello");
        assert_eq!(doc.count, 42);
    }

    #[test]
    fn test_parse_missing_delimiters() {
        let result = parse_yaml_front_matter::<TestDoc>("no delimiters here");
        assert!(result.is_err());
    }

    #[test]
    fn test_format_roundtrip() {
        let doc = TestDoc { title: "RT".into(), count: 7 };
        let formatted = format_yaml_front_matter(&doc, "").unwrap();
        let parsed: TestDoc = parse_yaml_front_matter(&formatted).unwrap();
        assert_eq!(doc, parsed);
    }

    #[test]
    fn test_format_preserves_body() {
        let doc = TestDoc { title: "X".into(), count: 1 };
        let body = "# Notes\nSome text";
        let output = format_yaml_front_matter(&doc, body).unwrap();
        assert!(output.contains("# Notes\nSome text"));
    }
}
