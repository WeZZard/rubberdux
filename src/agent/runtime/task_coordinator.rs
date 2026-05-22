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
// CompleteResult — outcome of completing a single background task
// ---------------------------------------------------------------------------

pub enum CompleteResult {
    GroupCompleted(CompletedGroup),
    Pending,
    Unknown(BackgroundTaskResult),
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
    ) -> Result<(), String> {
        // Check for duplicate task IDs before inserting any.
        for tid in task_ids {
            if self.task_to_group.contains_key(tid) {
                return Err(format!("Duplicate task ID: {}", tid));
            }
        }

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

        Ok(())
    }

    /// Record a completed task. Returns the group if all tasks in the group are done.
    pub fn complete(&mut self, result: BackgroundTaskResult) -> CompleteResult {
        let group_key = match self.task_to_group.remove(&result.task_id) {
            Some(k) => k,
            None => {
                log::warn!("Received result for unknown task: {}", result.task_id);
                return CompleteResult::Unknown(result);
            }
        };

        let group = match self.groups.get_mut(&group_key) {
            Some(g) => g,
            None => {
                log::warn!("No active group for key {}", group_key);
                return CompleteResult::Unknown(result);
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
            CompleteResult::GroupCompleted(CompletedGroup { group })
        } else {
            CompleteResult::Pending
        }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn get_mut(&mut self, asst_entry_id: usize) -> Option<&mut TaskGroup> {
        self.groups.get_mut(&asst_entry_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(task_id: &str) -> BackgroundTaskResult {
        BackgroundTaskResult {
            task_id: task_id.into(),
            content: format!("result for {}", task_id),
        }
    }

    #[test]
    fn test_complete_returns_pending_then_completed() {
        let mut set = TaskGroupSet::new();
        let tasks = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        set.register(1, &tasks, EntryOrigin::System, None).unwrap();

        match set.complete(make_result("t1")) {
            CompleteResult::Pending => {}
            other => panic!("Expected Pending, got {:?}", std::mem::discriminant(&other)),
        }

        match set.complete(make_result("t2")) {
            CompleteResult::Pending => {}
            other => panic!("Expected Pending, got {:?}", std::mem::discriminant(&other)),
        }

        match set.complete(make_result("t3")) {
            CompleteResult::GroupCompleted(completed) => {
                assert_eq!(completed.group.completed_results.len(), 3);
            }
            other => panic!("Expected GroupCompleted, got {:?}", std::mem::discriminant(&other)),
        }

        assert!(set.is_empty());
    }

    #[test]
    fn test_complete_unknown_task() {
        let mut set = TaskGroupSet::new();

        match set.complete(make_result("unknown")) {
            CompleteResult::Unknown(_) => {}
            other => panic!("Expected Unknown, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_register_duplicate_task_id() {
        let mut set = TaskGroupSet::new();
        let tasks = vec!["t1".to_string()];
        assert!(set.register(1, &tasks, EntryOrigin::System, None).is_ok());
        assert!(set.register(2, &tasks, EntryOrigin::System, None).is_err());
    }
}

pub struct CompletedGroup {
    pub group: TaskGroup,
}
