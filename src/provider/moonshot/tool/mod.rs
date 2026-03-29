pub mod prompt;
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
