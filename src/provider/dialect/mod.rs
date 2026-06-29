//! dialect — wire-protocol adapters that implement [`crate::provider::ModelApi`]
//! for a provider's HTTP surface.
//!
//! Each adapter is a skeleton here: it holds the resolved configuration and
//! exposes the `ModelApi` contract, but the real wire translation is filled by
//! the per-dialect implementation tasks.

pub mod anthropic_messages;
pub mod openai_chat_completions;
