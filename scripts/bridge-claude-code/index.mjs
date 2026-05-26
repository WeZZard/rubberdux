import { ClaudeCodeSession } from "toll-free-harness";
import { createInterface } from "readline";

const rl = createInterface({ input: process.stdin });
const pendingResponses = new Map();

function emit(event) {
    process.stdout.write(JSON.stringify(event) + "\n");
}

let session = null;

rl.on("line", async (line) => {
    let cmd;
    try {
        cmd = JSON.parse(line);
    } catch {
        return;
    }

    if (cmd.type === "start") {
        const args = cmd.args || [];

        session = new ClaudeCodeSession({
            prompt: cmd.prompt || "",
            args: args,
            cwd: cmd.cwd || process.cwd(),
        });

        session.onAskUserQuestion(async (event) => {
            const requestId = `q-${Date.now()}`;
            emit({
                type: "ask_user_question",
                requestId,
                text: event.text,
                questions: event.questions || [],
            });
            return new Promise((resolve) => {
                pendingResponses.set(requestId, resolve);
            });
        });

        session.onExitPlanMode(async (event) => {
            const requestId = `p-${Date.now()}`;
            emit({
                type: "plan_review",
                requestId,
                planText: event.planText || "",
            });
            return new Promise((resolve) => {
                pendingResponses.set(requestId, resolve);
            });
        });

        session.onPreToolUse("*", (request) => {
            emit({
                type: "progress",
                message: `Tool: ${request.toolName || "unknown"}`,
            });
        });

        session.onStop(() => {
            emit({ type: "completed", result: "Session completed" });
        });

        try {
            await session.run();
        } catch (err) {
            emit({ type: "failed", error: err.message || String(err) });
        }

        process.exit(0);
    }

    if (cmd.type === "answer_question" && cmd.requestId) {
        const resolve = pendingResponses.get(cmd.requestId);
        if (resolve) {
            pendingResponses.delete(cmd.requestId);
            resolve({ selectedIndex: cmd.selectedIndex || 0 });
        }
    }

    if (cmd.type === "approve_plan" && cmd.requestId) {
        const resolve = pendingResponses.get(cmd.requestId);
        if (resolve) {
            pendingResponses.delete(cmd.requestId);
            resolve({ decision: "approve" });
        }
    }

    if (cmd.type === "reject_plan" && cmd.requestId) {
        const resolve = pendingResponses.get(cmd.requestId);
        if (resolve) {
            pendingResponses.delete(cmd.requestId);
            resolve({ decision: "reject", feedback: cmd.feedback || "" });
        }
    }

    if (cmd.type === "cancel") {
        if (session) {
            session.stop();
        }
        process.exit(0);
    }
});
