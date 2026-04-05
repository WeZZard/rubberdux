use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::tool::bash::BashTool;
use crate::tool::{Tool, ToolOutcome};

pub struct MoonshotBashTool {
    inner: BashTool,
}

impl MoonshotBashTool {
    pub fn new() -> Self {
        Self { inner: BashTool }
    }
}

impl Tool for MoonshotBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("bash.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        self.inner.execute(arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_moonshot_bash_definition_has_behavioral_overlay() {
        let tool = MoonshotBashTool::new();
        let def = tool.definition();
        let desc = def.function.description.as_deref().unwrap_or("");
        assert!(
            desc.contains("delivered to you automatically"),
            "should contain Kimi behavioral instruction"
        );
    }
}
