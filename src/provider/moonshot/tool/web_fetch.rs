use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::{Tool, ToolOutcome};

pub struct MoonshotWebFetchTool {
    inner: WebFetchTool,
}

impl MoonshotWebFetchTool {
    pub fn new() -> Self {
        Self {
            inner: WebFetchTool,
        }
    }
}

impl Tool for MoonshotWebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("web_fetch.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        self.inner.execute(arguments)
    }
}
