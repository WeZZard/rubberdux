# Design: External Agent Architecture — Revised

> **For Claude:**
>
> This is a design document. Implementation plans will be created per phase after review.

**Goal:** Enable rubberdux to orchestrate external coding agents (Claude Code, Codex) running in VMs, handling their interactions through evidence-based resolution with human escalation as fallback.

---

## Architecture

The operator logic lives inside rubberdux's main agent. The same code runs in both modes:

**Host mode (development/testing):**

```
Rubberdux (main agent = operator)
       │
       ├── Handles human user messages (Telegram)
       ├── Operates external agents directly on host
       │      ├── toll-free-harness → Claude Code
       │      └── Codex app-server → Codex
       ├── Resolves interactions (evidence-based or human escalation)
       └── No VM involved
```

**VM mode (production):**

```
Host: Rubberdux (planner + orchestrator)
       │
       └──→ VM Instance
              │
              └── Rubberdux instance (same operator code)
                     ├── Operates external agents in VM
                     └── Forwards interactions to host via RPC
```

In host mode, the main agent IS the operator — it directly manages external agents and resolves their interactions. In VM mode, a rubberdux instance in the VM runs the same operator code. The operator logic is not a separate component; it's built into rubberdux itself.

Phases 1-2 (already committed) implement host mode. VM mode is layered on top later using the existing ComputerUse infrastructure.

---

## Interaction Handling

External agents produce three kinds of interactions: questions, plan reviews, and permission requests. The host resolves them through evidence, not LLM judgment.

### Questions (agent asks "Which database?")

```
External agent asks question
       │
       v
Injected into main conversation as user message:
  "[External agent interaction — task abc123]
   Claude Code asks: 'Which database?'
   Options: 0: PostgreSQL  1: MySQL  2: SQLite
   Find evidence in the codebase. Escalate if no ground truth."
       │
       v
Main LLM responds (short turn):
  → tool call: agent(explore, "Search for database config...")
       │
       v
Explore subagent runs in background → main loop idle → responsive
       │
       v
Subagent result injected:
  "Found: docker-compose.yml defines PostgreSQL, .env has DATABASE_URL=postgresql://..."
       │
       v
Main LLM responds (short turn):
  → tool call: interaction_respond(request_id: "q-123", select_option, index: 0)
       │
       v
Response flows: interaction_queue → RPC → VM → Claude Code continues
```

### Plan Review (agent produces implementation plan)

```
External agent produces plan
       │
       v
Injected into main conversation:
  "[External agent interaction — plan review]
   Plan (238 lines): [plan text]
   Spawn review subagents to check consistency and completeness."
       │
       v
Main LLM spawns review subagents (short turn) → main loop idle
       │
       v
Reviews come back with specific findings:
  "Reviewer 1: Missing error handling for network failures"
  "Reviewer 2: Plan is consistent with task stop condition"
       │
       v
Main LLM resolves:
  → interaction_respond(reject, feedback: "Missing error handling...")
  OR: interaction_respond(approve)
  OR: escalate to human (Telegram inline buttons)
```

### Permission Requests (agent needs to run command / edit file)

```
External agent requests permission
       │
       v
Injected as user message → Main LLM evaluates risk:
  - Safe operation → interaction_respond(grant)
  - Dangerous operation → escalate to human (Telegram buttons)
  - Uncertain → spawn explore subagent to check, then decide
```

### Human Escalation

When the main LLM cannot resolve with evidence, the interaction stays in the InteractionQueue. The user sees Telegram inline buttons and responds manually. Both paths (LLM tool call and human button click) use the same `interaction_queue.resolve()` code path.

---

## Resolution Paths

```
External agent interaction arrives at host
       │
       ├─── Path A: LLM-resolved (evidence-based)
       │    Main LLM → spawn subagent → find evidence → interaction_respond tool
       │
       ├─── Path B: Human-resolved (escalation)
       │    Interaction stays in queue → Telegram inline buttons → human clicks
       │
       └─── Path C: Policy-resolved (auto, future optimization)
            Pre-configured rules auto-resolve without LLM (e.g., auto-approve safe commands)
```

---

## The `interaction_respond` Tool

A new tool the main LLM calls to resolve pending interactions:

```json
{
  "name": "interaction_respond",
  "parameters": {
    "request_id": "the pending interaction's request ID",
    "response_type": { "enum": ["select_option", "approve_plan", "reject_plan", "grant_permission", "deny_permission"] },
    "index": "option index (for select_option)",
    "feedback": "rejection reason (for reject_plan, deny_permission)"
  }
}
```

Calls `interaction_queue.resolve(request_id, response)` — same code path as Telegram callback handler.

---

## VM Mode (deferred)

When isolation is needed, the host dispatches to a VM where a rubberdux instance runs the same operator code. The VM instance manages external agents locally and forwards interactions to the host via RPC.

### RPC Protocol Extension (when VM mode is implemented)

```rust
// VM → Host: external agent needs user input
AgentToHost::ExternalInteraction {
    task_id: String,
    request: UIInteractionRequest,
}

// Host → VM: user (or LLM) responded
HostToAgent::InteractionResponse {
    request_id: String,
    response: UIInteractionResponse,
}
```

Not needed for host mode — interactions flow through in-process channels.

---

## Responsiveness

External agent interactions are handled by the main loop. The main LLM turn is short (spawn subagent → return). Heavy work runs in background subagents. Between LLM turns, the main loop's `select!` is idle and processes user messages immediately.

Optimization opportunities if responsiveness becomes an issue:
1. Batch multiple pending interactions into a single LLM turn
2. Pre-configured policies that auto-resolve without LLM
3. Parallel loop as last resort — deferred until profiling shows bottleneck

---

## What Exists (Phases 1-2, committed)

- `src/agent/external/mod.rs` — ExternalAgentEvent, UIInteractionRequest/Response, spawn_external_agent_session
- `src/agent/external/claude_code.rs` — ClaudeCodeSession (bridge process, JSONL, response channel)
- `src/agent/external/interaction_queue.rs` — InteractionQueue (pending interactions, oneshot resolution)
- `scripts/bridge-claude-code/` — Node.js bridge wrapping toll-free-harness
- `src/channel/adapter/telegram.rs` — Inline keyboard buttons, callback query handler
- `src/tool/agent.rs` — SubagentType::External dispatch with agent_name routing

---

## What to Build Next (host mode first, VM mode later)

### Immediate (host mode — external agents run on host)

1. **`interaction_respond` tool** — new tool the main LLM calls to resolve interactions. Resolves via same queue code path as Telegram buttons.

2. **Interaction injection into main conversation** — when UIInteraction events arrive, inject as user messages with structured context + instructions for the LLM.

3. **Codex adapter** — `src/agent/external/codex.rs` connecting to Codex app-server via WebSocket JSON-RPC. Follows ClaudeCodeSession pattern. Runs on host.

4. **Plan-first workflow** — external agents enter plan mode. Plans flow through review subagents before execution approval.

### Deferred (VM mode — external agents run in VM)

5. **RPC protocol extension** — `ExternalInteraction` and `InteractionResponse` message types.

6. **VM provisioning** — install Claude Code, Codex, toll-free-harness, Node.js in VM image.

7. **VM dispatch** — route SubagentType::External through ComputerUse VM when isolation is requested.

---

## Design Decisions (resolved)

### 1. Convention Layer + Message Injection

The injected message wording is specified in the system prompt via the convention layer. A new convention registers:
- **Guidance**: instructions for how the LLM should handle external agent interactions (spawn subagents for ground truth, escalate if insufficient)
- **Guardrail**: post-guardrail that validates the LLM appends the correct structured message when handling interactions

### 2. AskUserQuestion Interactions (separate workflow)

When an external agent asks a question:

```
Question arrives
       │
       v
Safety gate: check if question involves dangerous operations
(e.g., payment, credential access, destructive actions)
       │
   ┌───┴───┐
   │       │
 safe    dangerous
   │       │
   v       v
Spawn     Escalate to human immediately
explore   (do NOT attempt to answer)
subagents
   │
   v
Evaluate: does collected ground truth sufficiently answer?
   │
   ┌───┴───┐
   │       │
  yes     no
   │       │
   v       v
Answer   Escalate to human
with     (present findings so far)
evidence
```

### 3. Plan Review (separate workflow)

When an external agent produces a plan:

```
Plan arrives
       │
       v
Spawn review subagents (4 checks):
  (i)   Verify plan format
  (ii)  Ensure plan is consistent
  (iii) Review overall design
  (iv)  Confirm plan is complete
       │
       v
All pass → present plan to human user for final approval
Issues found → surface issues to human user immediately
```

Plan reviews always surface to the human. The review subagents don't auto-approve — they prepare findings for the human to make the final decision.

### 4. VM Image Management

Pre-built images. Always.
