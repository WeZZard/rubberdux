use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use rubberdux::agent::entry::EntryHistory;
use rubberdux::agent::runtime::compaction::{CompactionStrategy, EvictOldestTurns};
use rubberdux::provider::moonshot::{Message, UserContent};

/// Test that compaction reduces history size when token budget is exceeded.
#[tokio::test]
async fn test_compaction_reduces_history_size() {
    let mut history = EntryHistory::new();
    history.push_system(Message::System {
        content: "system prompt".into(),
    });

    // Add multiple turns
    for i in 0..10 {
        let user_id = history.push_user(Message::User {
            content: UserContent::Text(format!("message {}", i)),
        });
        history.push_assistant(
            user_id,
            Message::Assistant {
                content: Some(format!("response {}", i)),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
    }

    let original_len = history.len();
    assert!(original_len > 5);

    let strategy = EvictOldestTurns;
    strategy.compact(
        &mut history,
        100,   // token budget
        10000, // current tokens (way over budget)
    );

    // History should be smaller
    assert!(history.len() < original_len, "History should be compacted");

    // System message must survive
    assert!(
        matches!(&history.entries()[0].message, Message::System { .. }),
        "System message must be preserved"
    );
}

/// Test that compaction preserves the system message even with aggressive eviction.
#[tokio::test]
async fn test_compaction_preserves_system_message() {
    let mut history = EntryHistory::new();
    history.push_system(Message::System {
        content: "system prompt".into(),
    });

    // Add multiple turns
    for i in 0..10 {
        let user_id = history.push_user(Message::User {
            content: UserContent::Text(format!("message {}", i)),
        });
        history.push_assistant(
            user_id,
            Message::Assistant {
                content: Some(format!("response {}", i)),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
    }

    let original_len = history.len();
    assert!(original_len > 5);

    let strategy = EvictOldestTurns;
    strategy.compact(
        &mut history,
        100,   // token budget
        10000, // current tokens (way over budget)
    );

    // History should be smaller
    assert!(history.len() < original_len, "History should be compacted");

    // System message must survive
    assert!(
        matches!(&history.entries()[0].message, Message::System { .. }),
        "System message must be preserved"
    );

    // Verify no orphaned tool results (this test doesn't add tools, but verify structure)
    for entry in history.entries() {
        if let Message::Tool { .. } = &entry.message {
            let parent_exists = history
                .entries()
                .iter()
                .any(|e| Some(e.id) == entry.parent_id);
            assert!(parent_exists, "Orphaned Tool result found");
        }
    }
}
