use std::collections::BTreeMap;

use crate::provider::moonshot::tool::{FunctionDefinition, ToolDefinition};
use crate::tool::ToolOutcome;

const BASH_JSON: &str = include_str!("bash.json");
const WEB_FETCH_JSON: &str = include_str!("web_fetch.json");

/// Applies Kimi-specific overrides to standard tool definitions and inserts
/// provider-owned custom tools. Keyed by tool name for natural deduplication.
pub fn override_tool_definitions(
    mut defaults: BTreeMap<String, ToolDefinition>,
) -> BTreeMap<String, ToolDefinition> {
    // Override standard tools with Kimi-specific descriptions
    for json in [BASH_JSON, WEB_FETCH_JSON] {
        if let Ok(def) = serde_json::from_str::<ToolDefinition>(json) {
            defaults.insert(def.function.name.clone(), def);
        }
    }

    // Add custom provider tools
    defaults.insert(
        "$web_search".to_owned(),
        ToolDefinition {
            r#type: "builtin_function".to_owned(),
            function: FunctionDefinition {
                name: "$web_search".to_owned(),
                description: None,
                parameters: None,
            },
        },
    );

    defaults
}

/// Returns true if this tool is owned by the Moonshot provider.
pub fn is_provider_tool(name: &str) -> bool {
    name == "$web_search"
}

/// Optional format override. Returns None to use default format_tool_outcome.
/// Currently Moonshot uses the default for all tools.
pub fn format_tool_outcome(_name: &str, _outcome: &ToolOutcome) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_override_tool_definitions_includes_behavioral_overlay() {
        let defaults = crate::tool::default_tool_definitions();
        let defs = override_tool_definitions(defaults);
        let bash = defs.get("bash").expect("bash should be present");
        let desc = bash.function.description.as_deref().unwrap_or("");
        assert!(desc.contains("bash command"), "should contain domain description");
        assert!(desc.contains("delivered to you automatically"), "should contain Kimi behavioral instruction");
    }

    #[test]
    fn test_override_tool_definitions_includes_provider_tools() {
        let defaults = crate::tool::default_tool_definitions();
        let defs = override_tool_definitions(defaults);
        assert!(defs.contains_key("$web_search"), "should include $web_search");
    }

    #[test]
    fn test_is_provider_tool() {
        assert!(is_provider_tool("$web_search"));
        assert!(!is_provider_tool("bash"));
        assert!(!is_provider_tool("web_fetch"));
    }
}
