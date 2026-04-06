pub mod bash;
pub mod web_fetch;
pub mod web_search;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub r#type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    pub id: String,
    pub r#type: String,
    pub function: FunctionCall,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub depends_on: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

impl ToolDefinition {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            r#type: "function".to_owned(),
            function: FunctionDefinition {
                name: name.to_owned(),
                description: Some(description.to_owned()),
                parameters: Some(parameters),
            },
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.r#type == "builtin_function"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_call_depends_on_serde() {
        // Without depends_on
        let tc = ToolCall {
            index: None,
            id: "call_1".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
            depends_on: None,
        };
        let json = serde_json::to_value(&tc).unwrap();
        assert!(json.get("depends_on").is_none(), "depends_on: None should be omitted");

        // With depends_on
        let tc2 = ToolCall {
            index: None,
            id: "call_2".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_1".into()),
        };
        let json2 = serde_json::to_value(&tc2).unwrap();
        assert_eq!(json2["depends_on"], "call_1");

        // Roundtrip without depends_on field in JSON
        let raw = r#"{"id":"call_3","type":"function","function":{"name":"x","arguments":"{}"}}"#;
        let parsed: ToolCall = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.depends_on, None);
    }
}
