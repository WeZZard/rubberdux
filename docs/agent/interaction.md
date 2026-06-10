# Agent-Initiated Interaction Vocabulary

## Context

An app is a persistent agent that works autonomously while the user observes
and approves (whiteboard spec D1). When an agent needs a human decision it
raises an *interaction*. The whiteboard specification fixes a single
interaction vocabulary so that any agent works with zero new UI code per app
(whiteboard spec [D2](../apps/whiteboard.md)): customization lives in the data
the agent supplies, not in bespoke per-app code or a UI DSL.

This document specifies the unified vocabulary (`AgentInteraction`,
`InteractionResponse`) defined in `src/agent/interaction.rs`, and how it
bridges losslessly to the pre-existing external-agent interaction types
(`UIInteractionRequest` / `UIInteractionResponse`) in
`src/agent/external/mod.rs`.

## The Vocabulary

`AgentInteraction` is the fixed set of four interaction primitives an agent may
raise. Every variant carries a `request_id` (correlates the request with its
response) and an `app_id` (the app/agent task that raised it). Both are
exposed through accessors so call sites need not match on the variant.

| Variant    | Purpose                                              | Data |
|------------|------------------------------------------------------|------|
| `Approval` | A yes/no decision on a described action or a plan.   | `prompt` |
| `Question` | An open question, optionally with suggested options. | `text`, `options` |
| `Choice`   | A forced selection among mutually exclusive options. | `prompt`, `options: Vec<ChoiceOption>` |
| `Preview`  | Present a generated artifact for acknowledgement.    | `prompt`, `artifact: PreviewArtifact` |

`InteractionResponse` is the fixed set of replies. Every variant carries the
`request_id` it answers. `Approved` / `Declined` answer an `Approval` and carry
the same `ApprovalFlavor` (`Permission` / `Plan`) as the request, so the legacy
bridge can reconstruct the exact legacy response variant without information
loss; `Answered` answers a `Question` or `Choice` by selecting an option index
(or a free-form reply); `Acknowledged` answers a `Preview`.

Each type derives `serde::Serialize` / `serde::Deserialize` with a stable
`#[serde(tag = "kind")]` discriminator, so the on-the-wire discriminator is
decoupled from the Rust variant name and survives refactors.

## Lossless Bridge to the Legacy External-Agent Types

The external-agent path (Claude Code, Codex) predates this vocabulary and
speaks `UIInteractionRequest` / `UIInteractionResponse`. To keep that behavior
unchanged while letting the rest of the system speak one vocabulary, `From`
conversions map every legacy variant to a unified variant and back, with no
loss of information for any variant that has a legacy counterpart:

| Legacy request          | Unified                                   |
|-------------------------|-------------------------------------------|
| `PermissionRequest`     | `Approval { flavor: Permission }`         |
| `PlanApproval`          | `Approval { flavor: Plan }`               |
| `Question`              | `Question`                                |

| Legacy response         | Unified                                          |
|-------------------------|--------------------------------------------------|
| `PermissionGranted`     | `Approved { flavor: Permission }`                |
| `PermissionDenied`      | `Declined { flavor: Permission }`                |
| `PlanApproved`          | `Approved { flavor: Plan }`                       |
| `PlanRejected`          | `Declined { flavor: Plan }`                       |
| `SelectedOption`        | `Answered { selected: Some(index) }`             |

Because `Approved` / `Declined` carry the `ApprovalFlavor`, the response bridge
is lossless in both directions: every one of the five legacy response variants
round-trips (legacy → unified → legacy) to itself, which the unit tests in
`src/agent/external/mod.rs` assert.

### Degradations

Two new request primitives — `Choice` and `Preview` — have no legacy
counterpart. The reverse request bridge degrades them to the closest legacy
shape rather than dropping them: `Choice` → `Question` and `Preview` →
`PermissionRequest`. The forward bridge is therefore exhaustive over the three
legacy request variants and round-trips for them; the degraded shapes are not
expected on the external-agent path, which raises only the three legacy-mapped
variants.

The reverse response bridge mirrors this. The two unified-only response shapes
with no legacy counterpart degrade to the closest legacy shape: an `Answered`
that carries only a free-form `reply` (no selected index) degrades to
`SelectedOption { index: 0 }`, and `Acknowledged` (a `Preview` acknowledgement,
which the external-agent path never produces) degrades to `PermissionGranted`.
The five legacy-mapped response variants are unaffected and round-trip exactly.

## Raising an Interaction Through the Port

`LoopEvent::RaiseInteraction` and `InputPort::raise_interaction` let any input
source push an `AgentInteraction` into an agent loop, mirroring the existing
`UserMessage` / `ContextUpdate` event idioms in
`src/agent/runtime/port.rs`. The agent-facing tools that construct these
interactions (`request_approval`, `ask_question`, …) are a separate concern and
are specified elsewhere.

## Rejected Alternatives

- **Per-app bespoke interaction types.** Fights convention-over-configuration
  and does not scale to thousands of apps (whiteboard spec D2).
- **A declarative interaction DSL.** Needs an interpreter and is premature
  (whiteboard spec D2).
- **Replacing the legacy types outright.** Would change observable Claude
  Code / Codex behavior; the lossless bridge preserves it instead.
