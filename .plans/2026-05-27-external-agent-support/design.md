# Design: External Agent Support — Claude Code and Codex Integration

> **For Claude:**
>
> This is a design document. Implementation plans will be created per phase after review.

**Goal:** Extend the agent tool to dispatch work to external coding agents (Claude Code via toll-free-harness, Codex via app-server), forward their user interactions to the main agent, track progress, and audit completion.

---

## Architecture

**Before:**

```
┌──────────────────────────────────────────────────────┐
│                    Agent Loop                        │
│                                                      │
│  LLM → AgentTool → ┌─────────────────────────────┐  │
│                     │ Internal Subagent Dispatch   │  │
│                     │                             │  │
│                     │  Explore ──→ in-process loop│  │
│                     │  Plan ─────→ in-process loop│  │
│                     │  GP ───────→ in-process loop│  │
│                     │  ComputerUse → VM RPC       │  │
│                     └─────────────────────────────┘  │
│                                                      │
│  SubagentResult ←── oneshot channel ←── loop exits   │
└──────────────────────────────────────────────────────┘
```

**After:**

```
┌──────────────────────────────────────────────────────────────────────┐
│                          Agent Loop                                  │
│                                                                      │
│  LLM → AgentTool → ┌────────────────────────────────────────────┐   │
│                     │           Agent Dispatch                   │   │
│                     │                                            │   │
│                     │  subagent_type:                             │   │
│                     │    Explore/Plan/GP → in-process loop        │   │
│                     │    ComputerUse → VM RPC                    │   │
│                     │    External + agent_name → adapter registry │   │
│                     │      "claude_code" → toll-free-harness     │   │
│                     │      "codex" → app-server JSON-RPC         │   │
│                     └────────────────────────────────────────────┘   │
│                                                                      │
│  SubagentResult ←── unified channel ←── session exits                │
│                                                                      │
│  UIInteractionRequest ←── adapter ←── agent needs user input         │
│       │                                                              │
│       v                                                              │
│  ┌──────────────────────────────┐                                    │
│  │    Interaction Queue         │                                    │
│  │    (pending user decisions)  │                                    │
│  └──────────┬───────────────────┘                                    │
│             │                                                        │
│    Main agent notifies user                                          │
│    User responds via channel UI                                      │
│             │                                                        │
│             v                                                        │
│  UIInteractionResponse → adapter → external agent continues          │
│                                                                      │
│  Progress/ToolUse events → trajectory recorder                       │
│  Completed → audit subagent verifies                                 │
└──────────────────────────────────────────────────────────────────────┘
```
```

---

## Core Abstractions

### ExternalAgentSession trait

A unified async interface over different agent backends:

```rust
#[async_trait]
pub trait ExternalAgentSession: Send {
    /// Start the session with an initial prompt
    async fn start(&mut self, prompt: &str) -> Result<(), Error>;

    /// Send a follow-up prompt
    async fn send_prompt(&mut self, prompt: &str) -> Result<(), Error>;

    /// Respond to a pending UIInteractionRequest
    async fn respond(&mut self, response: UIInteractionResponse) -> Result<(), Error>;

    /// Get the next event from the agent
    async fn next_event(&mut self) -> Option<ExternalAgentEvent>;

    /// Cancel the session
    async fn cancel(&mut self);
}
```

The `respond()` method handles all user interaction responses. The adapter translates `UIInteractionResponse` to the agent's native format.

### ExternalAgentAdapter trait

Factory for creating sessions:

```rust
pub trait ExternalAgentAdapter: Send + Sync {
    fn name(&self) -> &str;
    async fn create_session(&self, cwd: &Path) -> Result<Box<dyn ExternalAgentSession>, Error>;
}
```

Adapters are registered at startup and selected by `agent_name` parameter.

### ExternalAgentEvent

Events streamed from external agents. `UIInteraction` events go to the queue; others are logged/tracked.

```rust
pub enum ExternalAgentEvent {
    UIInteraction(UIInteractionRequest),
    Progress { message: String },
    Completed { result: String },
    Failed { error: String },
}
```

### SubagentType extension

Add a single `External` variant to `SubagentType`:

```rust
pub enum SubagentType {
    Explore,
    Plan,
    GeneralPurpose,
    ComputerUse,
    External,          // NEW — dispatches to external agent adapters
}
```

The specific external agent (Claude Code, Codex, etc.) is selected by a separate `agent_name` parameter in the tool call, not by the subagent type. The `External` type tells the dispatch layer to route through the external agent adapter system rather than the internal subagent system.

---

## Claude Code Adapter (toll-free-harness)

### Communication model: Node.js sidecar

Rubberdux spawns a Node.js child process for each Claude Code session. The child runs a bridge script that wraps toll-free-harness and speaks JSONL over stdin/stdout.

```
rubberdux (Rust) ←── stdin/stdout JSONL ──→ bridge.js (Node.js)
                                                 │
                                                 └── toll-free-harness
                                                          │
                                                          └── Claude Code (PTY)
```

### Bridge script protocol

**Commands (stdin → bridge):**

```jsonl
{"type": "start", "prompt": "Fix the failing tests", "args": ["--model", "opus"], "cwd": "/path"}
{"type": "answer_question", "selectedIndex": 0}
{"type": "approve_plan"}
{"type": "reject_plan", "feedback": "Missing error handling"}
{"type": "send_prompt", "prompt": "Now add tests"}
{"type": "cancel"}
```

**Events (bridge → stdout):**

```jsonl
{"type": "ask_user_question", "text": "...", "options": [{"label": "...", "description": "..."}]}
{"type": "plan_review", "planText": "..."}
{"type": "tool_use", "toolName": "Bash", "input": {"command": "cargo test"}}
{"type": "tool_result", "toolName": "Bash", "output": "...", "isError": false}
{"type": "progress", "message": "Reading file src/main.rs"}
{"type": "completed", "result": "All tests pass. Changes committed."}
{"type": "failed", "error": "Session terminated unexpectedly"}
```

### Bridge implementation

Located at `scripts/bridge-claude-code.js` (~100 lines):
1. Read config from first stdin line
2. Create `ClaudeCodeSession` via toll-free-harness
3. Wire `onAskUserQuestion` → emit `ask_user_question` event
4. Wire `onExitPlanMode` → emit `plan_review` event
5. Wire `onPreToolUse`/`onPostToolUse` → emit `tool_use`/`tool_result` events
6. Wire `onStop` → emit `completed` event
7. Read commands from stdin, dispatch to session methods
8. Exit on session completion

### Rust adapter

`src/agent/external/claude_code.rs`:
- Spawns `node scripts/bridge-claude-code.js` as child process
- Reads events from stdout (tokio `BufReader` + `lines()`)
- Writes commands to stdin (tokio `BufWriter`)
- Implements `ExternalAgentSession` trait
- Converts JSONL events to `ExternalAgentEvent` enum

---

## Codex Adapter (app-server)

### Communication model: JSON-RPC over WebSocket

Codex's app-server speaks JSON-RPC. Rubberdux connects as a WebSocket client:

```
rubberdux (Rust) ←── WebSocket JSON-RPC ──→ codex app-server
```

### Protocol mapping

| Rubberdux action | Codex JSON-RPC method |
|---|---|
| Start session | `initialize` → `thread/start` → `turn/start` |
| Send follow-up | `turn/start` (new turn in existing thread) |
| Answer question | Response to `item/tool/requestUserInput` server request |
| Approve file change | Response to `item/fileChange/requestApproval` |
| Approve command | Response to `item/commandExecution/requestApproval` |
| Cancel | `turn/interrupt` |

### Server → client events (approval requests)

When Codex needs user input, it sends a JSON-RPC **server request**:

| Server request | Maps to ExternalAgentEvent |
|---|---|
| `item/tool/requestUserInput` | `AskUserQuestion` |
| `item/commandExecution/requestApproval` | Permission approval (auto-approve or forward) |
| `item/fileChange/requestApproval` | Permission approval (auto-approve or forward) |
| `item/permissions/requestApproval` | Permission approval (auto-approve or forward) |

### Rust adapter

`src/agent/external/codex.rs`:
- Connects to `codex app-server --listen ws://127.0.0.1:PORT` (or starts daemon)
- Uses `tokio-tungstenite` for WebSocket client
- Implements JSON-RPC request/response/notification framing
- Implements `ExternalAgentSession` trait
- Maps server requests to `ExternalAgentEvent`

---

## Agent Tool Extension

### AgentTool dispatch

In `AgentTool::execute()`, the `External` type routes through the adapter registry:

```rust
SubagentType::External => {
    let agent_name = args["agent_name"].as_str().unwrap_or("claude_code");
    let adapter = self.external_adapters.get(agent_name)?;
    let session = adapter.create_session(prompt, cwd).await?;
    let handle = spawn_external_agent_session(
        task_id, session,
        self.child_agent_tx.clone(),
        self.interaction_queue.clone(),
    );
    ToolOutcome::Subagent { handle }
}
```

The `agent_name` parameter selects which external agent adapter to use. Available adapters are registered at startup (e.g., "claude_code", "codex").

### External session lifecycle

`spawn_external_agent_session()` creates a tokio task that:
1. Drives the session event loop (`session.next_event()`)
2. `UIInteractionRequest` events → pushed to the interaction queue (user processes them)
3. `Progress`/`ToolUse`/`ToolResult` events → logged to trajectory recorder
4. `Completed`/`Failed` → sends `SubagentResult` via the existing oneshot channel
5. The main loop receives the result through the existing `child_agent_rx` path

---

## User Interaction Forwarding

External agent interactions (questions, plan approvals, permission requests) are unified as **UIInteractionRequest/Response** pairs. Each adapter translates between our native format and the external agent's native format.

### Native UI interaction types

```rust
pub enum UIInteractionRequest {
    Question {
        request_id: String,
        agent_task_id: String,
        text: String,
        options: Vec<QuestionOption>,
    },
    PlanApproval {
        request_id: String,
        agent_task_id: String,
        plan_text: String,
    },
    PermissionRequest {
        request_id: String,
        agent_task_id: String,
        description: String,
    },
}

pub enum UIInteractionResponse {
    SelectedOption { request_id: String, index: usize },
    PlanApproved { request_id: String },
    PlanRejected { request_id: String, feedback: String },
    PermissionGranted { request_id: String },
    PermissionDenied { request_id: String, reason: String },
}
```

### Adapter translation

Each adapter translates bidirectionally:

**Claude Code adapter (toll-free-harness):**
- Inbound: `onAskUserQuestion` event → `UIInteractionRequest::Question`
- Outbound: `UIInteractionResponse::SelectedOption { index }` → `{ selectedIndex: index }`
- Inbound: `onExitPlanMode` event → `UIInteractionRequest::PlanApproval`
- Outbound: `UIInteractionResponse::PlanApproved` → `{ decision: "approve" }`

**Codex adapter (app-server):**
- Inbound: `item/tool/requestUserInput` → `UIInteractionRequest::Question`
- Outbound: `UIInteractionResponse::SelectedOption` → JSON-RPC response
- Inbound: `item/commandExecution/requestApproval` → `UIInteractionRequest::PermissionRequest`
- Outbound: `UIInteractionResponse::PermissionGranted` → JSON-RPC approval response

### Interaction Queue

External agent interactions go into a **pending queue**. The human user processes them — not the main LLM.

```
External agent (native event)
       │
       v
  ┌─────────┐
  │ Adapter  │  translates to our native UIInteractionRequest
  └────┬─────┘
       │
       v
  ┌───────────────────────────────┐
  │   Interaction Queue           │
  │                               │
  │   [1] Claude Code: "Which     │
  │       approach?" (A / B)      │
  │   [2] Codex: approve bash     │
  │       command "cargo test"    │
  │   [3] Claude Code: approve    │
  │       plan (238 lines)        │
  └───────────────┬───────────────┘
                  │
                  v
  Main agent notifies user:
    "3 pending agent interactions. Review them?"
                  │
                  v
  User opens secondary menu / dialog
  (Telegram: inline buttons, summary message)
                  │
                  v
  User responds to each (or in batch)
                  │
                  v
  UIInteractionResponse
       │
       v
  ┌─────────┐
  │ Adapter  │  translates to agent's native format
  └────┬─────┘
       │
       v
  External agent continues
```

### Roles

- **Main agent**: Unifies messages from all external agents into the common `UIInteractionRequest` format. Notifies the user about the queue. Does NOT decide on behalf of the user.
- **Telegram channel**: Customizes the appearance — how the queue looks, how the user interacts (inline buttons, reply menu, dialog). Channel-specific UX.
- **Human user**: Reviews pending interactions, processes them individually or in batch.

### Queue behavior

- Interactions accumulate while external agents wait (agents are blocked on the pending response)
- The user is notified when new items arrive (e.g., Telegram message: "Agent X is asking: ...")
- The user can respond immediately or defer
- Batch processing: user can review all pending items and respond in one pass
- If the user doesn't respond, the external agent remains blocked (with optional timeout)

### Telegram UX (channel-specific)

The Telegram channel renders the queue as:
- **Notification**: Summary message when new interactions arrive
- **Inline buttons**: Quick responses for simple choices (option A / option B)
- **Expand dialog**: Tap to see full context (plan text, command details)
- **Batch view**: "Review all N pending" button that shows all items

The main agent sends a unified notification; the channel processor decides how to render it.

---

## Progress Tracking + Audit

### Tracking

External agent events (`ToolUse`, `ToolResult`, `Progress`) are:
1. Logged via the trajectory recorder
2. Stored in the subagent's session directory
3. Available for the main agent to observe (injected as context when relevant)

### Audit on completion

When an external agent's session completes:
1. The completion result is injected into the main conversation (existing `SubagentResult` path)
2. The main agent spawns an audit subagent (internal, read-only) to verify the work
3. The audit subagent reads the external agent's output/changes and evaluates against the original goal
4. The audit result is injected back into the main conversation

The audit subagent is an existing internal `Explore` or `Plan` type — no new infrastructure needed. The main LLM decides when and whether to audit, based on the task's importance.

---

## Hypotheses

1. **toll-free-harness JSONL bridge**: The bridge script approach (Node.js child process with JSONL stdin/stdout) has not been tested. Need to verify that toll-free-harness's async event handlers can be wired to synchronous stdin/stdout without deadlocks.
   **Evidence supporting**: toll-free-harness uses async callbacks that resolve independently. The bridge reads stdin in a separate readline loop and writes to stdout on events. Standard pattern for Node.js CLI tools.
   **Verification**: Build the bridge script and test with a real Claude Code session.

2. **Codex app-server WebSocket stability**: The app-server is marked `[experimental]`. The protocol may change between versions.
   **Evidence supporting**: The `generate-json-schema` command exists, suggesting the protocol is stabilizing. The schema covers a comprehensive set of operations.
   **Verification**: Connect to a running app-server and perform a basic `initialize` → `thread/start` → `turn/start` cycle.

3. **Telegram inline buttons for interaction responses**: The Telegram Bot API supports inline keyboards for structured responses. Need to verify that the teloxide library can send inline keyboard markup and receive callback queries.
   **Evidence supporting**: teloxide has `InlineKeyboardMarkup` and `CallbackQuery` support (standard Bot API features).
   **Verification**: Send a test message with inline buttons and handle the callback.

---

## Human Verification Gate

**Criterion:** "External agent sessions consume real API credits (Claude Code uses Anthropic credits, Codex uses OpenAI credits)"
**Category:** Financial/Credit Authorization
**Reason:** Running external agents triggers paid API calls. Testing requires real agent sessions that cost money.

---

## Implementation Phases

1. **Phase 1: Core abstractions + Claude Code adapter**
   - `ExternalAgentSession` trait and `ExternalAgentEvent` enum
   - Claude Code bridge script (Node.js)
   - `ClaudeCodeSession` Rust adapter
   - Wire into `AgentTool` dispatch
   - Basic result forwarding (Completed → SubagentResult)

2. **Phase 2: Interaction queue + user-facing UX**
   - `InteractionQueue` struct (pending requests, response dispatch)
   - Main agent notification when items arrive
   - Telegram channel: inline buttons / summary for interaction responses
   - `UIInteractionResponse` forwarding back through adapter to external agent

3. **Phase 3: Codex adapter**
   - JSON-RPC client over WebSocket
   - `CodexSession` Rust adapter
   - Protocol mapping (thread/start, turn/start, approval requests)

4. **Phase 4: Progress tracking + audit**
   - Event logging via trajectory recorder
   - Audit subagent spawning on completion
   - Progress injection as context messages

5. **Phase 5: Mode toggling**
   - Plan mode: approve/reject plan via interaction forwarding
   - Permission mode: auto-approve or forward permission requests
   - Configuration: per-session policy (auto-approve all, forward all, hybrid)

---

## Open Questions

1. **Bridge script location**: Should the Node.js bridge script live in the rubberdux repo (`scripts/`) or as a separate npm package? If in-repo, does rubberdux need Node.js as a build dependency?

2. **Codex app-server lifecycle**: Should rubberdux start the Codex app-server daemon itself, or expect it to already be running? The `codex app-server daemon` subcommand manages the lifecycle.

3. **Concurrent external agents**: Can the main agent run multiple external agent sessions simultaneously? The current `TaskGroupSet` supports multiple concurrent tasks — does the interaction forwarding work with multiple agents asking questions at the same time?

4. **Credential management**: External agents need their own API keys. How are these configured? Environment variables? Per-agent config?

---

## Verification

**Verification Approach:** Hybrid: automate and manual

**Verification Steps:**

Manual verification required because external agent sessions use real API credits and interact with real CLI tools:

1. **Phase 1**: Spawn Claude Code via toll-free-harness, send a simple prompt, verify result comes back to main conversation.
2. **Phase 2**: Trigger a Claude Code AskUserQuestion event, verify main LLM receives and answers it.
3. **Phase 3**: Start a Codex session via app-server, run a simple task, verify completion.
4. **Phase 4**: After an external session completes, verify an audit subagent is spawned and its assessment injected.

Automated tests cover the abstractions and protocol handling (mock external agents):

**Test Cases:**

1. ADD: `src/agent/external/mod.rs` (inline tests)
    **Test layer:** Unit
    **Specifies:** ExternalAgentEvent serialization/deserialization
    1. ADD: `test_event_roundtrip` — all event variants serialize and deserialize

2. ADD: `src/agent/external/claude_code.rs` (inline tests)
    **Test layer:** Integration
    **Specifies:** Bridge protocol parsing
    1. ADD: `test_parse_ask_question_event` — parse JSONL ask_user_question event
    2. ADD: `test_parse_completed_event` — parse JSONL completed event
    3. ADD: `test_serialize_answer_command` — serialize answer_question command

3. ADD: `src/agent/external/codex.rs` (inline tests)
    **Test layer:** Integration
    **Specifies:** JSON-RPC message framing
    1. ADD: `test_initialize_request` — build valid initialize JSON-RPC request
    2. ADD: `test_parse_approval_server_request` — parse commandExecution approval request
