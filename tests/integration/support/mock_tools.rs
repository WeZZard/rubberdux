use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use rubberdux::provider::moonshot::tool::ToolDefinition;
use rubberdux::tool::{BackgroundTaskResult, Tool, ToolOutcome};

// ---------------------------------------------------------------------------
// MockBackgroundTool — simulates a tool that returns a background task
// ---------------------------------------------------------------------------

/// Mock tool that returns a `ToolOutcome::Background` and allows test code
/// to trigger completion later via `complete()`.
#[derive(Clone)]
pub struct MockBackgroundTool {
    name: String,
    completion_tx: Arc<Mutex<Option<oneshot::Sender<BackgroundTaskResult>>>>,
}

impl MockBackgroundTool {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            completion_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Trigger completion from test code. Panics if no background task is pending.
    pub fn complete(&self, content: &str) {
        let tx = self
            .completion_tx
            .lock()
            .unwrap()
            .take()
            .expect("No pending background task to complete");
        let _ = tx.send(BackgroundTaskResult {
            task_id: format!("mock_{}", self.name),
            content: content.to_string(),
        });
    }
}

impl Tool for MockBackgroundTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            &self.name,
            &format!("A mock background tool: {}", self.name),
            serde_json::json!({}),
        )
    }

    fn execute<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        let name = self.name.clone();
        let tx = self.completion_tx.clone();
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            *tx.lock().unwrap() = Some(sender);
            ToolOutcome::Background {
                task_id: format!("mock_{}", name),
                output_path: std::path::PathBuf::from("/tmp"),
                receiver,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// MockTool — configurable tool with optional delay and dependency tracking
// ---------------------------------------------------------------------------

/// Mock tool that returns an immediate result with configurable delay.
/// Tracks execution order for testing dependent tool scheduling.
#[derive(Clone)]
pub struct MockTool {
    name: String,
    delay: Duration,
    execution_log: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl MockTool {
    pub fn new(name: &str, delay: Duration) -> Self {
        Self {
            name: name.to_string(),
            delay,
            execution_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn execution_order(&self) -> Vec<String> {
        self.execution_log
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn execution_times(&self) -> Vec<(String, Instant)> {
        self.execution_log.lock().unwrap().clone()
    }
}

impl Tool for MockTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            &self.name,
            &format!("A mock tool: {}", self.name),
            serde_json::json!({}),
        )
    }

    fn execute<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        let name = self.name.clone();
        let delay = self.delay;
        let log = self.execution_log.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            let now = Instant::now();
            log.lock().unwrap().push((name.clone(), now));
            ToolOutcome::Immediate {
                content: format!("Result from {}", name),
                is_error: false,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// MockErrorTool — always returns an error
// ---------------------------------------------------------------------------

pub struct MockErrorTool {
    name: String,
    error_message: String,
}

impl MockErrorTool {
    pub fn new(name: &str, error_message: &str) -> Self {
        Self {
            name: name.to_string(),
            error_message: error_message.to_string(),
        }
    }
}

impl Tool for MockErrorTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            &self.name,
            &format!("A mock error tool: {}", self.name),
            serde_json::json!({}),
        )
    }

    fn execute<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        let msg = self.error_message.clone();
        Box::pin(async move {
            ToolOutcome::Immediate {
                content: msg,
                is_error: true,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helper: build a registry with mock tools
// ---------------------------------------------------------------------------

pub fn build_registry_with(tools: Vec<Box<dyn Tool>>) -> rubberdux::tool::ToolRegistry {
    let mut registry = rubberdux::tool::ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}
