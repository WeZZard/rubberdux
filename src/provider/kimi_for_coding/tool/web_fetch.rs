use std::future::Future;
use std::pin::Pin;

use crate::provider::kimi_for_coding::tool::ToolDefinition;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::{Tool, ToolOutcome};

pub struct KimiForCodingWebFetchTool {
    inner: WebFetchTool,
}

impl KimiForCodingWebFetchTool {
    pub fn new() -> Self {
        Self {
            inner: WebFetchTool,
        }
    }
}

impl Tool for KimiForCodingWebFetchTool {
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
