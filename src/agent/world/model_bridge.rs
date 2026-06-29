//! model_bridge — the world ↔ neutral model-call conversion. See
//! docs/agent/world/ecs-runtime.md (Anthropic model-call mapping).
//!
//! The LIVE driver drives a model call through the config-selected
//! [`crate::provider::ModelApi`], whose vocabulary is the provider-domain neutral
//! [`ModelRequest`]/[`ModelResponse`]. This module is the SEAM that translates the
//! world's recorded types (`History`/`ToolSchema`/`ModelConfig` and the recorded
//! `Block`/`ModelMeta`) to and from that neutral vocabulary, so the provider stays
//! free of any world import (the conversion lives HERE, on the world side).
//!
//! The request `Fingerprint` is computed over the world's `(History, ToolSet,
//! ModelConfig)` BEFORE this conversion runs and is never affected by it; this
//! module only shapes the wire-bound request and reshapes the response back into
//! the exact `(Vec<Block>, ModelMeta)` the former world model client returned,
//! so the recorded events stay byte-identical (replay determinism).

use std::future::Future;
use std::pin::Pin;

use base64::Engine;

use super::history::{Block, History, ImageSource as WorldImageSource, Role as WorldRole, ToolSchema};
use super::inputs::{Capabilities, ModelMeta};
use super::world::{Effort as WorldEffort, ModelConfig};
use crate::error::Error;
use crate::provider::{
    ContentBlock, ContentMessage, Effort as NeutralEffort, ImageSource as NeutralImageSource,
    ModelApi, ModelInfo, ModelRequest, ModelResponse, Role as NeutralRole, Sampling, ToolSpec,
};

// ---------------------------------------------------------------------------
// world → neutral request
// ---------------------------------------------------------------------------

/// Build a neutral [`ModelRequest`] from the resolved world call: the assembled
/// `system` prompt, the `ModelConfig` sampling knobs, the resolved tool
/// declarations, and the (blob-resolved) `History`. Each world `Block` maps to its
/// neutral [`ContentBlock`], the world `Msg.role` to a neutral [`NeutralRole`], and
/// `ModelConfig` to [`Sampling`] — the same shaping the former `MessageBuilder`
/// performed, now expressed in the neutral vocabulary so any dialect adapter can
/// serialise it.
pub fn to_model_request(
    system: &str,
    params: &ModelConfig,
    tools: &[ToolSchema],
    history: &History,
) -> ModelRequest {
    ModelRequest {
        system: system.to_string(),
        messages: history
            .messages()
            .iter()
            .map(|msg| ContentMessage {
                role: to_neutral_role(msg.role),
                content: msg.content.iter().map(to_content_block).collect(),
            })
            .collect(),
        tools: tools.iter().map(to_tool_spec).collect(),
        sampling: Sampling {
            model: params.model.clone(),
            max_tokens: params.max_tokens,
            effort: to_neutral_effort(params.effort),
        },
    }
}

// ---------------------------------------------------------------------------
// neutral response → world
// ---------------------------------------------------------------------------

/// Reshape a neutral [`ModelResponse`] back into the exact `(Vec<Block>,
/// ModelMeta)` the former world model client returned, so the LIVE driver
/// records byte-identical `ModelResponded`/`Compacted` events (replay
/// determinism). Each neutral [`ContentBlock`] maps to its world `Block`, and the
/// response's `(usage, model_id, stop_reason, reasoning)` plus the opaque
/// `capabilities` (wrapped into the world's `Capabilities`) assemble the
/// [`ModelMeta`].
pub fn from_model_response(resp: ModelResponse) -> (Vec<Block>, ModelMeta) {
    let blocks = resp.blocks.into_iter().map(to_world_block).collect();
    let meta = ModelMeta {
        usage: resp.usage,
        model_id: resp.model_id,
        stop_reason: resp.stop_reason,
        // The world seam wraps the opaque passthrough capabilities into its own
        // `Capabilities` newtype — the field the recorded `ModelMeta` carries.
        capabilities: Capabilities(resp.capabilities),
        reasoning: resp.reasoning,
    };
    (blocks, meta)
}

// ---------------------------------------------------------------------------
// block / role / effort / tool translation
// ---------------------------------------------------------------------------

/// Translate a world `Block` into its neutral [`ContentBlock`] (request direction).
fn to_content_block(block: &Block) -> ContentBlock {
    match block {
        Block::Text { text } => ContentBlock::Text { text: text.clone() },
        Block::Reasoning { text, signature } => ContentBlock::Reasoning {
            text: text.clone(),
            signature: signature.clone(),
        },
        Block::ToolUse { id, name, input } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(to_content_block).collect(),
            is_error: *is_error,
        },
        Block::Image { source } => ContentBlock::Image {
            source: world_image_to_neutral(source),
        },
    }
}

/// Translate a neutral [`ContentBlock`] into its world `Block` (response direction).
fn to_world_block(block: ContentBlock) -> Block {
    match block {
        ContentBlock::Text { text } => Block::Text { text },
        ContentBlock::Reasoning { text, signature } => Block::Reasoning { text, signature },
        ContentBlock::ToolUse { id, name, input } => Block::ToolUse { id, name, input },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Block::ToolResult {
            tool_use_id,
            content: content.into_iter().map(to_world_block).collect(),
            is_error,
        },
        ContentBlock::Image { source } => Block::Image {
            source: neutral_image_to_world(source),
        },
    }
}

/// Map a world image source to the neutral one. The request is always built from
/// the BLOB-RESOLVED `History` (every `Blob{hash}` already restored to `Inline`),
/// so the `Inline → Base64` arm is the one that runs in practice; the `Blob` arm
/// is kept total (it cannot arise on the resolved path).
fn world_image_to_neutral(source: &WorldImageSource) -> NeutralImageSource {
    match source {
        WorldImageSource::Inline { mime, bytes } => NeutralImageSource::Base64 {
            media_type: mime.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        WorldImageSource::Blob { hash, mime } => NeutralImageSource::Url {
            url: format!("blob:{mime}:{}", hash.0),
        },
    }
}

/// Map a neutral image source back to the world's. A model response carries
/// `Base64` images (decoded back to the world `Inline` bytes); the `Url` arm has
/// no world inline/blob counterpart and is not produced by the dialects in
/// practice, so it degrades to empty-mime inline bytes to stay total.
fn neutral_image_to_world(source: NeutralImageSource) -> WorldImageSource {
    match source {
        NeutralImageSource::Base64 { media_type, data } => WorldImageSource::Inline {
            mime: media_type,
            // A malformed base64 payload (never produced by our dialects) decodes
            // to empty bytes rather than panicking — no unwrap/expect in the seam.
            bytes: base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .unwrap_or_default(),
        },
        NeutralImageSource::Url { url } => WorldImageSource::Inline {
            mime: String::new(),
            bytes: url.into_bytes(),
        },
    }
}

/// Map a world conversation role to the neutral one. World history carries only
/// `User`/`Assistant`; the system prompt is a separate request field.
fn to_neutral_role(role: WorldRole) -> NeutralRole {
    match role {
        WorldRole::User => NeutralRole::User,
        WorldRole::Assistant => NeutralRole::Assistant,
    }
}

/// Map the world's `Effort` knob to the neutral one.
fn to_neutral_effort(effort: WorldEffort) -> NeutralEffort {
    match effort {
        WorldEffort::Low => NeutralEffort::Low,
        WorldEffort::Medium => NeutralEffort::Medium,
        WorldEffort::High => NeutralEffort::High,
    }
}

/// Map a resolved world `ToolSchema` to a neutral [`ToolSpec`] declaration.
fn to_tool_spec(schema: &ToolSchema) -> ToolSpec {
    ToolSpec {
        name: schema.name.clone(),
        description: schema.description.clone(),
        input_schema: schema.input_schema.clone(),
    }
}

// ---------------------------------------------------------------------------
// Box<dyn ModelApi> adapter
// ---------------------------------------------------------------------------

/// Make an owned `Box<dyn ModelApi>` itself a [`ModelApi`], by forwarding to the
/// inner object. This lets the `WorldDriver` (generic over its model client `C:
/// ModelApi`) hold the config-selected `provider::selected_from_env()` value —
/// which is a `Box<dyn ModelApi>` whose concrete type is not nameable — as `C`,
/// while the offline tests still pass concrete stubs. The trait is local to this
/// crate, so this blanket-style impl for a foreign `Box` is permitted.
impl ModelApi for Box<dyn ModelApi> {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        (**self).turn(req)
    }

    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        (**self).list_models()
    }

    fn model(&self) -> &str {
        (**self).model()
    }
}

// ---------------------------------------------------------------------------
// test support
// ---------------------------------------------------------------------------

/// Build a neutral [`ModelResponse`] from the world `(Vec<Block>, ModelMeta)` an
/// offline mock wants to return — the inverse of [`from_model_response`]. A mock
/// `ModelApi` keeps expressing its canned reply in world `Block`/`ModelMeta`
/// fixtures and converts here, so a test's intent is unchanged across the seam.
#[cfg(test)]
pub(crate) fn to_model_response(blocks: Vec<Block>, meta: ModelMeta) -> ModelResponse {
    ModelResponse {
        blocks: blocks.iter().map(to_content_block).collect(),
        stop_reason: meta.stop_reason,
        usage: meta.usage,
        model_id: meta.model_id,
        reasoning: meta.reasoning,
        capabilities: meta.capabilities.0,
    }
}
