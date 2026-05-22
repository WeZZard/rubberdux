use std::future::Future;
use std::pin::Pin;

use crate::agent::entry::Entry;
use crate::error::Error;

pub trait ChannelProcessor: Send + Sync {
    fn channel_name(&self) -> &str;

    fn process_outbound<'a>(
        &'a self,
        entry: &'a mut Entry,
        channel_metadata: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>>;

    fn tools(&self) -> Vec<Box<dyn crate::tool::Tool>> {
        vec![]
    }
}
