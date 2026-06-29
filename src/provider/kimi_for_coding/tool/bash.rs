use std::future::Future;
use std::pin::Pin;

use crate::provider::kimi_for_coding::tool::ToolDefinition;
use crate::tool::bash::BashTool;
use crate::tool::{Tool, ToolOutcome};

pub struct KimiForCodingBashTool {
    inner: BashTool,
}

impl KimiForCodingBashTool {
    pub fn new() -> Self {
        Self { inner: BashTool }
    }
}

impl Tool for KimiForCodingBashTool {
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
    fn test_kimi_for_coding_bash_definition_has_behavioral_overlay() {
        let tool = KimiForCodingBashTool::new();
        let def = tool.definition();
        let desc = def.function.description.as_deref().unwrap_or("");
        assert!(
            desc.contains("delivered to you automatically"),
            "should contain Kimi behavioral instruction"
        );
    }
}
