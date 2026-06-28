//! history — see docs/agent/world/ecs-runtime.md

use serde::{Deserialize, Serialize};

use super::world::{Effort, ModelConfig};
use crate::error::Error;

/// Opaque structured JSON, used for tool-call inputs. Aliased so the domain
/// reads as "a tool's input is JSON" rather than leaking the serde library name.
pub type Json = serde_json::Value;

// ---------------------------------------------------------------------------
// History — the canonical, Anthropic-block-shaped conversation
// ---------------------------------------------------------------------------

/// The entity's conversation as an ordered list of role-tagged messages.
/// Block-structured so it maps 1:1 onto the Anthropic Messages schema; this is
/// the basis of a reproducible request `Fingerprint`. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct History(pub Vec<Msg>);

impl History {
    /// The messages, borrowed in order. The ordering is the conversation order
    /// the model consumes, never re-sorted.
    pub fn messages(&self) -> &[Msg] {
        &self.0
    }
}

/// One conversation message: a role and its ordered content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Msg {
    pub role: Role,
    pub content: Vec<Block>,
}

/// Anthropic conversation roles. Serialises lowercase (`user`/`assistant`) to
/// match the Messages wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Content address of an externalized blob. The hash string is opaque at this
/// level — computing it is `BlobStore`'s responsibility (BL-store). `Ord` and
/// `PartialOrd` are derived so a `BlobHash` can be used as a `BTreeMap`/`BTreeSet`
/// key without breaking determinism (no hash-map randomness). See
/// docs/agent/world/ecs-runtime.md §146-147 + §1415-1434.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlobHash(pub String);

/// Source of a `Block::Image` history entry. Small payloads (at or under
/// `Caps.blob_inline_cap`, or when the cap is 0) are carried `Inline`; larger
/// ones are externalized and the log/snapshot stores only the `Blob` hash.
///
/// `Inline.bytes` uses `Vec<u8>`. serde_json emits a JSON integer array for
/// `Vec<u8>` — deterministic, no extra codec dependency, and round-trips exactly.
/// Base64 encoding was considered but rejected: it requires an additional crate
/// and the compactness gain is irrelevant because inline blobs are, by definition,
/// small (under the cap).
///
/// See docs/agent/world/ecs-runtime.md §1415-1434 (two-stage blob design).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Bytes are carried inline (≤ `Caps.blob_inline_cap`, or cap is 0).
    Inline { mime: String, bytes: Vec<u8> },
    /// Bytes exceeded the cap; only the content-address hash is stored here.
    /// The bytes live in the blob store (BL-store), keyed by this hash.
    Blob { hash: BlobHash, mime: String },
}

/// A single content block, shaped to the Anthropic Messages content schema. The
/// `type` discriminator and field order are fixed so serialisation is canonical
/// (the reproducible-`Fingerprint` requirement). `Reasoning` carries its opaque
/// `signature` and serialises as a `thinking` block, echoed back unchanged.
/// `Image` carries an `ImageSource` (inline bytes or a content-addressed blob
/// hash) so the log/snapshot never carry large raw bytes past the inline cap.
/// See docs/agent/world/ecs-runtime.md §141-148 (Block + Anthropic model-call mapping).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Json,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<Block>,
        #[serde(default)]
        is_error: bool,
    },
    /// A reasoning block, serialised as Anthropic `thinking` and echoed back
    /// verbatim with its `signature`.
    #[serde(rename = "thinking")]
    Reasoning {
        #[serde(rename = "thinking")]
        text: String,
        signature: String,
    },
    /// An image block. `source` is either inline bytes (≤ `Caps.blob_inline_cap`)
    /// or a content-addressed blob hash (externalized, BL-store). Additive variant:
    /// logs written before this variant still deserialise (unknown variants are
    /// rejected by serde, so logs with `Image` blocks require this version or later).
    /// See docs/agent/world/ecs-runtime.md §146-147 + §1415-1434.
    Image { source: ImageSource },
}

/// A `Block::ToolResult` specifically — the shape a resolved tool slot carries.
pub type ToolResult = Block;

// ---------------------------------------------------------------------------
// ToolSchema — one tool declaration in the Anthropic `/v1/messages` `tools` shape
// ---------------------------------------------------------------------------

/// One tool DECLARATION offered to a model call, in the Anthropic `tools` wire
/// shape: `{ name, description, input_schema }` where `input_schema` is the JSON
/// Schema the model's `tool_use.input` must satisfy. This is how a request tells
/// the model a tool EXISTS so it can request it; the resolved tool's
/// `input_schema` MUST mirror the args its live executor parses, so a model-
/// produced `tool_use.input` flows straight into that executor. Field order is
/// fixed so a request body serialises canonically. See
/// docs/agent/world/ecs-runtime.md (Anthropic model-call mapping).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Json,
}

// ---------------------------------------------------------------------------
// MessageBuilder — History + ModelConfig + system → POST /v1/messages request
// ---------------------------------------------------------------------------

/// Serialises a model call to the Anthropic `POST /v1/messages` request body:
/// the assembled `system` prompt, the `History` blocks as `messages`, and the
/// `ModelConfig` as `model`/`max_tokens`/`thinking`/`output_config.effort`.
///
/// The builder NEVER reads the wall clock: any date the `system` prompt embeds
/// must be assembled by the caller from `Resources.wall`, so the serialised
/// request is identical on replay. See docs/agent/world/ecs-runtime.md
/// (Anthropic model-call mapping).
pub struct MessageBuilder<'a> {
    system: &'a str,
    model: &'a ModelConfig,
    history: &'a History,
    tools: &'a [ToolSchema],
}

impl<'a> MessageBuilder<'a> {
    pub fn new(system: &'a str, model: &'a ModelConfig, history: &'a History) -> Self {
        MessageBuilder {
            system,
            model,
            history,
            // A tool-less call by default: the empty slice serialises away (see
            // `MessagesRequest.tools`), keeping a request body byte-identical to a
            // pre-tools call so its replay fingerprint is unchanged.
            tools: &[],
        }
    }

    /// Offer `tools` to this model call (the Anthropic `tools` declarations). The
    /// request body carries them so the model can request a tool; a tool-less
    /// call (the default empty set) serialises with NO `tools` key — byte-identical
    /// to before, keeping replay/fingerprint stable. See
    /// docs/agent/world/ecs-runtime.md.
    pub fn with_tools(mut self, tools: &'a [ToolSchema]) -> Self {
        self.tools = tools;
        self
    }

    /// Build the request value. Fallible only because JSON serialisation is, in
    /// principle, fallible; for these types it does not fail in practice.
    pub fn build(&self) -> Result<Json, Error> {
        let request = MessagesRequest {
            model: &self.model.model,
            max_tokens: self.model.max_tokens,
            system: self.system,
            messages: self.history.messages(),
            tools: self.tools,
            thinking: ThinkingConfig { kind: "adaptive" },
            output_config: OutputConfig {
                effort: self.model.effort,
            },
        };
        Ok(serde_json::to_value(&request)?)
    }
}

/// The Anthropic `/v1/messages` request body. A dedicated struct so field order
/// and naming are explicit rather than assembled ad hoc. `tools` is OMITTED when
/// empty (`skip_serializing_if`) so a tool-less call's body is byte-identical to a
/// pre-tools call — the replay/fingerprint-stability requirement.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [Msg],
    #[serde(skip_serializing_if = "<[ToolSchema]>::is_empty")]
    tools: &'a [ToolSchema],
    thinking: ThinkingConfig,
    output_config: OutputConfig,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: Effort,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Block::Image round-trip tests (BL-types VC-1.2)
    // -----------------------------------------------------------------------

    /// An `Inline` image carries its bytes in the log. serde_json emits a JSON
    /// integer array for `Vec<u8>` — deterministic and round-trips exactly.
    #[test]
    fn block_image_inline_round_trips() {
        let block = Block::Image {
            source: ImageSource::Inline {
                mime: "image/png".into(),
                bytes: vec![137, 80, 78, 71], // PNG magic bytes
            },
        };
        let json = serde_json::to_string(&block).expect("serialise");
        let back: Block = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(block, back);

        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "image", "Block::Image serialises as type=image");
        assert_eq!(v["source"]["type"], "inline");
        assert_eq!(v["source"]["mime"], "image/png");
        assert!(
            v["source"]["bytes"].is_array(),
            "Vec<u8> serialises as a JSON integer array (deterministic)"
        );
    }

    /// A `Blob` image carries only the content-address hash; bytes live in the
    /// blob store (BL-store). The log/snapshot never carry the raw bytes.
    #[test]
    fn block_image_blob_round_trips() {
        let block = Block::Image {
            source: ImageSource::Blob {
                hash: BlobHash("sha256:abc123def456".into()),
                mime: "image/jpeg".into(),
            },
        };
        let json = serde_json::to_string(&block).expect("serialise");
        let back: Block = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(block, back);

        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "image");
        assert_eq!(v["source"]["type"], "blob");
        assert_eq!(v["source"]["hash"], "sha256:abc123def456");
        assert_eq!(v["source"]["mime"], "image/jpeg");
    }

    /// `BlobHash` is `Ord` so it can be a key in a `BTreeMap`/`BTreeSet`.
    #[test]
    fn blob_hash_is_ord() {
        let mut hashes = vec![
            BlobHash("sha256:zzz".into()),
            BlobHash("sha256:aaa".into()),
            BlobHash("sha256:mmm".into()),
        ];
        hashes.sort();
        assert_eq!(hashes[0], BlobHash("sha256:aaa".into()));
        assert_eq!(hashes[2], BlobHash("sha256:zzz".into()));
    }

    fn fixed_history() -> History {
        History(vec![
            Msg {
                role: Role::Assistant,
                content: vec![
                    Block::Reasoning {
                        text: "let me think".into(),
                        signature: "sig-abc".into(),
                    },
                    Block::ToolUse {
                        id: "tu_1".into(),
                        name: "get_weather".into(),
                        input: serde_json::json!({ "city": "Paris" }),
                    },
                ],
            },
            Msg {
                role: Role::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: vec![Block::Text {
                        text: "sunny".into(),
                    }],
                    is_error: false,
                }],
            },
        ])
    }

    #[test]
    fn history_serialises_to_canonical_anthropic_json() {
        // A fixed History serialises to an EXACT canonical Anthropic shape with
        // deterministic field order: a Reasoning block as `thinking` carrying its
        // signature, and a ToolResult inside the following USER message.
        let expected = concat!(
            r#"[{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"let me think","signature":"sig-abc"},"#,
            r#"{"type":"tool_use","id":"tu_1","name":"get_weather","input":{"city":"Paris"}}"#,
            r#"]},{"role":"user","content":["#,
            r#"{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"text","text":"sunny"}],"is_error":false}"#,
            r#"]}]"#,
        );
        let actual = serde_json::to_string(&fixed_history()).expect("serialise history");
        assert_eq!(actual, expected);
    }

    #[test]
    fn history_round_trips() {
        let history = fixed_history();
        let json = serde_json::to_string(&history).expect("serialise");
        let back: History = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(history, back);
    }

    #[test]
    fn message_builder_maps_to_v1_messages_shape() {
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::High,
        };
        let history = fixed_history();
        let request = MessageBuilder::new("today is 2026-06-27", &model, &history)
            .build()
            .expect("build request");

        assert_eq!(request["model"], "claude-x");
        assert_eq!(request["max_tokens"], 1024);
        assert_eq!(request["system"], "today is 2026-06-27");
        assert_eq!(request["thinking"]["type"], "adaptive");
        assert_eq!(request["output_config"]["effort"], "high");

        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        // ToolResult rides inside a USER message (Anthropic shape).
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
    }

    /// [W3-tool-decl] A `MessageBuilder` carrying the `set_value` tool serialises a
    /// request body whose `tools` array holds the `set_value` declaration, and whose
    /// `input_schema` matches the W2 `set_value` executor's args
    /// (`{ "surface_ops": [ { op:"set_value", surface, element, value, base_version } ] }`)
    /// — so a model `tool_use.input` built to it flows straight into the executor.
    #[test]
    fn message_builder_serialises_the_set_value_tool_schema() {
        use crate::agent::world::surface::set_value_tool_schema;
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::High,
        };
        let history = fixed_history();
        let tools = vec![set_value_tool_schema()];
        let request = MessageBuilder::new("sys", &model, &history)
            .with_tools(&tools)
            .build()
            .expect("build request");

        // The body now carries a `tools` array with the set_value declaration.
        let tools_json = request["tools"].as_array().expect("tools array present");
        assert_eq!(tools_json.len(), 1);
        assert_eq!(tools_json[0]["name"], "set_value");
        assert!(tools_json[0]["description"].is_string(), "a tool declares a description");

        // Its input_schema mirrors the W2 executor's args shape exactly.
        let schema = &tools_json[0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert!(
            schema["required"]
                .as_array()
                .expect("required array")
                .iter()
                .any(|r| r == "surface_ops"),
            "the schema requires `surface_ops` — the projection envelope key the executor reads"
        );
        let op_item = &schema["properties"]["surface_ops"]["items"];
        assert_eq!(
            op_item["properties"]["op"]["const"], "set_value",
            "each op is pinned to the `set_value` tag the SurfaceOp encoding uses"
        );
        for field in ["op", "surface", "element", "value", "base_version"] {
            assert!(
                op_item["properties"].get(field).is_some(),
                "the set_value op schema must declare `{field}` (a W2 executor arg)"
            );
        }
    }

    /// [W3-tool-decl] A tool-less call (the default, and an explicit EMPTY tool
    /// slice) serialises with NO `tools` key — byte-identical to a pre-tools request
    /// body so its replay fingerprint is unchanged (replay/fingerprint stability).
    #[test]
    fn message_builder_omits_tools_when_empty_byte_identical() {
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::High,
        };
        let history = fixed_history();

        // The default tool-less call: no `tools` key at all.
        let no_tools = MessageBuilder::new("sys", &model, &history)
            .build()
            .expect("build no-tools");
        assert!(
            no_tools.get("tools").is_none(),
            "a tool-less call must omit the `tools` key entirely"
        );

        // Explicitly attaching an EMPTY tool slice is byte-identical.
        let empty: Vec<ToolSchema> = Vec::new();
        let with_empty = MessageBuilder::new("sys", &model, &history)
            .with_tools(&empty)
            .build()
            .expect("build empty-tools");
        assert_eq!(
            serde_json::to_string(&no_tools).expect("serialise no-tools"),
            serde_json::to_string(&with_empty).expect("serialise empty-tools"),
            "an empty ToolSet must serialise byte-identically (no `tools` key)"
        );
    }
}
