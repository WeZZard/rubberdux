use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rubberdux::agent::entry::Entry;
use rubberdux::channel::processor::ChannelProcessor;
use rubberdux::error::Error;
use tokio::sync::Mutex;

pub struct MockChannelProcessor {
    name: String,
    call_count: Arc<AtomicUsize>,
    should_fail: bool,
    entries_received: Arc<Mutex<Vec<usize>>>,
}

impl MockChannelProcessor {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            call_count: Arc::new(AtomicUsize::new(0)),
            should_fail: false,
            entries_received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn failing(name: &str) -> Self {
        Self {
            name: name.into(),
            call_count: Arc::new(AtomicUsize::new(0)),
            should_fail: true,
            entries_received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }

    pub async fn entries_received(&self) -> Vec<usize> {
        self.entries_received.lock().await.clone()
    }
}

impl ChannelProcessor for MockChannelProcessor {
    fn channel_name(&self) -> &str {
        &self.name
    }

    fn process_outbound<'a>(
        &'a self,
        entry: &'a mut Entry,
        _channel_metadata: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>> {
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            self.entries_received.lock().await.push(entry.id);

            if self.should_fail {
                Err(Error::Workspace("mock processor failure".into()))
            } else {
                Ok(())
            }
        })
    }
}
