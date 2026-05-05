use std::collections::HashMap;

use crate::agent::entry::EntryOrigin;
use crate::tool::BackgroundTaskResult;

// ---------------------------------------------------------------------------
// TaskGroup — tracks a batch of background tasks for one assistant turn
// ---------------------------------------------------------------------------

pub struct TaskGroup {
    pub origin: EntryOrigin,
    pub asst_entry_id: usize,
    pub remaining: usize,
    pub completed_results: Vec<BackgroundTaskResult>,
    pub channel_metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// TaskGroupSet — tracks all active task groups
// ---------------------------------------------------------------------------

pub struct TaskGroupSet {
    groups: HashMap<usize, TaskGroup>,
    task_to_group: HashMap<String, usize>,
}

impl TaskGroupSet {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            task_to_group: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        asst_entry_id: usize,
        task_ids: &[String],
        origin: EntryOrigin,
        channel_metadata: Option<serde_json::Value>,
    ) {
        for tid in task_ids {
            self.task_to_group.insert(tid.clone(), asst_entry_id);
        }

        if !self.groups.contains_key(&asst_entry_id) {
            self.groups.insert(
                asst_entry_id,
                TaskGroup {
                    origin,
                    asst_entry_id,
                    remaining: task_ids.len(),
                    completed_results: Vec::new(),
                    channel_metadata,
                },
            );
        } else if let Some(group) = self.groups.get_mut(&asst_entry_id) {
            group.remaining += task_ids.len();
            if group.channel_metadata.is_none() {
                group.channel_metadata = channel_metadata;
            }
        }
    }

    /// Record a completed task. Returns the group if all tasks in the group are done.
    pub fn complete(&mut self, result: BackgroundTaskResult) -> Option<CompletedGroup> {
        let group_key = match self.task_to_group.remove(&result.task_id) {
            Some(k) => k,
            None => {
                log::warn!("Received result for unknown task: {}", result.task_id);
                return None;
            }
        };

        let group = match self.groups.get_mut(&group_key) {
            Some(g) => g,
            None => {
                log::warn!("No active group for key {}", group_key);
                return None;
            }
        };

        log::info!(
            "Task {} completed ({} bytes), {}/{} in group",
            result.task_id,
            result.content.len(),
            group.completed_results.len() + 1,
            group.remaining + group.completed_results.len(),
        );

        group.completed_results.push(result);
        group.remaining -= 1;

        if group.remaining == 0 {
            let group = self.groups.remove(&group_key).unwrap();
            Some(CompletedGroup { group })
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn get_mut(&mut self, asst_entry_id: usize) -> Option<&mut TaskGroup> {
        self.groups.get_mut(&asst_entry_id)
    }
}

pub struct CompletedGroup {
    pub group: TaskGroup,
}
