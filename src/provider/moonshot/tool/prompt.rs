use std::collections::HashMap;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::tool::ToolOutcome;

const BASH_JSON: &str = include_str!("bash.json");
const WEB_FETCH_JSON: &str = include_str!("web_fetch.json");

/// Provider-specific tool definition overrides, keyed by tool name.
fn overrides() -> HashMap<&'static str, &'static str> {
    [("bash", BASH_JSON), ("web_fetch", WEB_FETCH_JSON)].into()
}

/// Assembles tool definitions with provider-first JSON resolution.
/// For each default tool, if a Kimi-specific override exists, use it.
/// Then appends provider-only custom tools ($web_search).
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let defaults = crate::tool::default_tool_definitions();
    let overrides = overrides();

    let mut defs: Vec<ToolDefinition> = defaults
        .into_iter()
        .map(|def| {
            if let Some(json) = overrides.get(def.function.name.as_str()) {
                serde_json::from_str(json).unwrap_or(def)
            } else {
                def
            }
        })
        .collect();

    // Add custom provider tools
    // $web_search is a Kimi builtin with no parameters
    defs.push(ToolDefinition {
        r#type: "builtin_function".to_owned(),
        function: crate::provider::moonshot::tool::FunctionDefinition {
            name: "$web_search".to_owned(),
            description: None,
            parameters: None,
        },
    });

    defs
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
    fn test_tool_definitions_includes_behavioral_overlay() {
        let defs = tool_definitions();
        let bash = defs.iter().find(|d| d.function.name == "bash").unwrap();
        let desc = bash.function.description.as_deref().unwrap_or("");
        assert!(
            desc.contains("bash command"),
            "should contain domain description"
        );
        assert!(
            desc.contains("do NOT sleep, poll"),
            "should contain Kimi behavioral instruction"
        );
    }

    #[test]
    fn test_tool_definitions_includes_provider_tools() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(
            names.contains(&"$web_search"),
            "should include $web_search"
        );
    }

    #[test]
    fn test_is_provider_tool() {
        assert!(is_provider_tool("$web_search"));
        assert!(!is_provider_tool("bash"));
        assert!(!is_provider_tool("web_fetch"));
    }
}
