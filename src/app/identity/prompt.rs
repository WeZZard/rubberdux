//! LLM prompt construction for identity derivation.
//!
//! Builds the system + user messages sent to the Moonshot API to obtain a
//! constrained JSON identity response. See `docs/app/identity.md`.

use super::{COLOR_PALETTE, SYMBOL_ALLOWLIST};
use crate::provider::moonshot::{Message, UserContent};

/// Build the system message that instructs the model to return a constrained
/// JSON identity object.
pub fn system_message() -> Message {
    let symbols = SYMBOL_ALLOWLIST.join(", ");
    let colors = COLOR_PALETTE.join(", ");

    let content = format!(
        "You are an app-identity assistant. \
        Given a task description, return a JSON object with exactly three fields:\n\
        - \"title\": a short descriptive title for the task (max 40 characters, plain text, \
          no punctuation at the end)\n\
        - \"sf_symbol\": one SF Symbol name chosen from this exact allowlist: [{symbols}]\n\
        - \"color\": one hex color string chosen from this exact palette: [{colors}]\n\
        \n\
        Return ONLY the JSON object. No explanation, no markdown fences.",
        symbols = symbols,
        colors = colors,
    );

    Message::System { content }
}

/// Build the user message for the given task string.
pub fn user_message(task: &str) -> Message {
    Message::User {
        content: UserContent::Text(format!("Task: {}", task)),
    }
}
