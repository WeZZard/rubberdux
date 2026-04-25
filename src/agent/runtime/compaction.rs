use crate::agent::entry::EntryHistory;
use crate::provider::moonshot::Message;

/// Strategy for compacting history when token budget is exceeded.
pub trait CompactionStrategy: Send + Sync {
    /// Compact the history to fit within the token budget.
    /// `current_tokens` is the actual prompt token count from the last LLM response.
    fn compact(&self, history: &mut EntryHistory, token_budget: usize, current_tokens: usize);
}

/// Default: evict oldest complete conversation turns, preserving the system message.
///
/// A "turn" is a User message followed by all its children (Assistant, Tool results).
/// Turns are evicted from oldest first, always keeping entries[0] (system message).
pub struct EvictOldestTurns;

impl CompactionStrategy for EvictOldestTurns {
    fn compact(&self, history: &mut EntryHistory, token_budget: usize, current_tokens: usize) {
        if current_tokens <= token_budget {
            return;
        }

        // Verify entries[0] is the system message; bail if not.
        let first_is_system = history
            .entries()
            .first()
            .map(|e| matches!(&e.message, Message::System { .. }))
            .unwrap_or(false);
        if !first_is_system {
            log::info!("compaction: entries[0] is not a system message, skipping");
            return;
        }

        // Collect turns starting from entries[1].
        // A turn starts with a User message that has parent_id = None.
        let entries = history.entries();
        let mut turns: Vec<Vec<usize>> = Vec::new();

        for entry in entries.iter().skip(1) {
            if matches!(&entry.message, Message::User { .. }) && entry.parent_id.is_none() {
                // Start a new turn.
                turns.push(vec![entry.id]);
            } else if let Some(turn) = turns.last_mut() {
                // Append to current turn.
                turn.push(entry.id);
            }
            // Entries before the first User (other than system) are ignored.
        }

        let total_entries = entries.len();
        let mut remaining_tokens = current_tokens;

        for turn in &turns {
            if remaining_tokens <= token_budget {
                break;
            }

            let estimated_turn_tokens = turn.len() * current_tokens / total_entries;

            log::info!(
                "compaction: evicting turn with {} entries (est. {} tokens), ids: {:?}",
                turn.len(),
                estimated_turn_tokens,
                turn
            );

            history.remove_entries(turn);
            remaining_tokens = remaining_tokens.saturating_sub(estimated_turn_tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::moonshot::tool::{FunctionCall, ToolCall};
    use crate::provider::moonshot::{Message, UserContent};

    /// Helper to build test history.
    fn build_test_history() -> EntryHistory {
        let mut h = EntryHistory::new();
        // Entry 0: System
        h.push_system(Message::System {
            content: "system".into(),
        });
        // Turn 1: User -> Assistant -> Tool
        let u1 = h.push_user(Message::User {
            content: UserContent::Text("q1".into()),
        });
        let a1 = h.push_assistant(
            u1,
            Message::Assistant {
                content: Some("answer1".into()),
                reasoning_content: None,
                tool_calls: Some(vec![ToolCall {
                    index: None,
                    id: "t1".into(),
                    r#type: "function".into(),
                    function: FunctionCall {
                        name: "bash".into(),
                        arguments: "{}".into(),
                    },
                    depends_on: None,
                }]),
                partial: None,
            },
        );
        h.push_tool(
            a1,
            Message::Tool {
                tool_call_id: "t1".into(),
                name: None,
                content: "result1".into(),
            },
        );
        // Turn 2: User -> Assistant (no tools)
        let u2 = h.push_user(Message::User {
            content: UserContent::Text("q2".into()),
        });
        h.push_assistant(
            u2,
            Message::Assistant {
                content: Some("answer2".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        h // 6 entries total
    }

    #[test]
    fn test_evict_preserves_system_message() {
        let mut h = build_test_history();
        let strategy = EvictOldestTurns;
        // Force compaction with tiny budget
        strategy.compact(&mut h, 10, 1000);
        // System message must survive
        assert!(matches!(&h.entries()[0].message, Message::System { .. }));
    }

    #[test]
    fn test_evict_removes_complete_turns() {
        let mut h = build_test_history();
        assert_eq!(h.len(), 6);
        let strategy = EvictOldestTurns;
        // Budget that requires evicting 1 turn
        // 1000 tokens / 6 entries = 167 per entry. Turn 1 has 3 entries = 500 tokens.
        // After evicting turn 1: 1000 - 500 = 500 tokens, 3 entries remain (System + Turn 2)
        strategy.compact(&mut h, 600, 1000);
        // Should have evicted turn 1 (User, Assistant, Tool = 3 entries)
        // Remaining: System + Turn 2 (User, Assistant) = 3 entries
        assert_eq!(h.len(), 3);
        assert!(matches!(&h.entries()[0].message, Message::System { .. }));
        // No orphaned Tool results
        for entry in h.entries() {
            if matches!(&entry.message, Message::Tool { .. }) {
                let parent_exists = h.entries().iter().any(|e| e.id == entry.parent_id.unwrap());
                assert!(parent_exists, "Orphaned Tool result found");
            }
        }
    }

    #[test]
    fn test_evict_does_not_break_pairing() {
        let mut h = build_test_history();
        let strategy = EvictOldestTurns;
        strategy.compact(&mut h, 100, 1000);
        // After aggressive compaction, every remaining Tool result must have its Assistant parent
        for entry in h.entries() {
            if let Message::Tool { .. } = &entry.message {
                let parent_id = entry.parent_id.unwrap();
                let parent = h.entries().iter().find(|e| e.id == parent_id);
                assert!(
                    parent.is_some(),
                    "Tool result at id {} has no parent",
                    entry.id
                );
                assert!(
                    matches!(&parent.unwrap().message, Message::Assistant { .. }),
                    "Tool result parent is not an Assistant"
                );
            }
        }
    }

    #[test]
    fn test_no_compaction_when_under_budget() {
        let mut h = build_test_history();
        let original_len = h.len();
        let strategy = EvictOldestTurns;
        strategy.compact(&mut h, 2000, 1000);
        assert_eq!(h.len(), original_len);
    }
}
