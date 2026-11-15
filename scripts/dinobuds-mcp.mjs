#!/usr/bin/env node
// Zero-dependency stdio JSON-RPC 2.0 MCP Server for DinoBuds
import readline from "node:readline";

const ENDPOINT = "http://127.0.0.1:4747/agent";
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const TOOLS = [
  {
    name: "dino_report_progress",
    description:
      "Report live agent task progress to the floating DinoBuds desktop HUD.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Unique session identifier" },
        name: { type: "string", description: "Display name of the agent" },
        task: {
          type: "string",
          description: "Short description of the current action",
        },
        current_step: {
          type: "number",
          description: "Current step number (e.g. 2)",
        },
        total_steps: {
          type: "number",
          description: "Total steps in the task (e.g. 5)",
        },
        step_label: {
          type: "string",
          description: "Short step label (e.g. 'Building', 'Testing')",
        },
        status: {
          type: "string",
          enum: ["running", "waiting_permission", "queued", "done"],
          description: "Agent status",
        },
        workspace_path: {
          type: "string",
          description: "Optional path to the workspace directory",
        },
      },
      required: ["id", "name", "task"],
    },
  },
  {
    name: "dino_ask_permission",
    description:
      "Prompt the user with an Approve / Deny card in the DinoBuds HUD and block until they click a button.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Unique session identifier" },
        name: { type: "string", description: "Display name of the agent" },
        task: { type: "string", description: "Context for the permission" },
        question: {
          type: "string",
          description:
            "Command or question requiring user approval in DinoBuds",
        },
        current_step: { type: "number", description: "Current step number" },
        total_steps: { type: "number", description: "Total steps" },
        workspace_path: {
          type: "string",
          description: "Optional path to the workspace directory",
        },
      },
      required: ["id", "name", "task", "question"],
    },
  },
  {
    name: "dino_complete_task",
    description: "Mark an agent session as done in the DinoBuds HUD.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Unique session identifier" },
        name: { type: "string", description: "Display name of the agent" },
        task: { type: "string", description: "Completion summary message" },
        total_steps: { type: "number", description: "Total steps completed" },
        workspace_path: {
          type: "string",
          description: "Optional path to the workspace directory",
        },
      },
      required: ["id", "name", "task"],
    },
  },
];

async function postAgentUpdate(payload) {
  const res = await fetch(ENDPOINT, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      updated_at: Math.floor(Date.now() / 1000),
      ...payload,
    }),
  });
  if (!res.ok) {
    throw new Error(`DinoBuds HTTP ${res.status}: ${await res.text()}`);
  }
  return res.json();
}

async function pollDecision(id) {
  while (true) {
    await sleep(500);
    const res = await fetch(`${ENDPOINT}/${encodeURIComponent(id)}`);
    if (!res.ok) {
      throw new Error(`Failed polling decision for ${id}: HTTP ${res.status}`);
    }
    const session = await res.json();
    if (session.decision === true || session.decision === false) {
      return session.decision;
    }
  }
}

async function handleToolCall(name, args = {}) {
  if (name === "dino_report_progress") {
    const payload = {
      id: String(args.id || "agent-1"),
      name: String(args.name || "Antigravity"),
      task: String(args.task || "Working..."),
      status: String(args.status || "running"),
      current_step: Number(args.current_step ?? 1),
      total_steps: Number(args.total_steps ?? 1),
      step_label: String(args.step_label || "Running"),
      question: null,
      workspace_path: args.workspace_path ? String(args.workspace_path) : null,
    };
    await postAgentUpdate(payload);
    return { ok: true, id: payload.id, status: payload.status };
  }

  if (name === "dino_ask_permission") {
    const id = String(args.id || "agent-1");
    const payload = {
      id,
      name: String(args.name || "Antigravity"),
      task: String(args.task || "Waiting for user approval"),
      status: "waiting_permission",
      current_step: Number(args.current_step ?? 1),
      total_steps: Number(args.total_steps ?? 1),
      step_label: "Permission",
      question: String(args.question || "Approve this action?"),
      workspace_path: args.workspace_path ? String(args.workspace_path) : null,
      decision: null,
    };
    await postAgentUpdate(payload);
    const approved = await pollDecision(id);
    return { approved };
  }

  if (name === "dino_complete_task") {
    const total = Number(args.total_steps ?? 1);
    const payload = {
      id: String(args.id || "agent-1"),
      name: String(args.name || "Antigravity"),
      task: String(args.task || "Task completed"),
      status: "done",
      current_step: total,
      total_steps: total,
      step_label: "Completed",
      question: null,
      workspace_path: args.workspace_path ? String(args.workspace_path) : null,
    };
    await postAgentUpdate(payload);
    return { ok: true, id: payload.id, status: "done" };
  }

  throw new Error(`Unknown tool: ${name}`);
}

function sendResponse(id, result) {
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n"
  );
}

function sendError(id, code, message) {
  process.stdout.write(
    JSON.stringify({
      jsonrpc: "2.0",
      id,
      error: { code, message },
    }) + "\n"
  );
}

const rl = readline.createInterface({
  input: process.stdin,
  terminal: false,
});

rl.on("line", async (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;

  let msg;
  try {
    msg = JSON.parse(trimmed);
  } catch {
    return;
  }

  // Notifications have no id
  if (msg.id === undefined || msg.id === null) {
    return;
  }

  try {
    if (msg.method === "initialize") {
      sendResponse(msg.id, {
        protocolVersion:
          msg.params?.protocolVersion || "2024-11-05",
        capabilities: {
          tools: {},
        },
        serverInfo: {
          name: "dinobuds-mcp",
          version: "0.1.0",
        },
      });
    } else if (msg.method === "ping") {
      sendResponse(msg.id, {});
    } else if (msg.method === "tools/list") {
      sendResponse(msg.id, { tools: TOOLS });
    } else if (msg.method === "tools/call") {
      const toolName = msg.params?.name;
      const toolArgs = msg.params?.arguments || {};
      try {
        const output = await handleToolCall(toolName, toolArgs);
        sendResponse(msg.id, {
          content: [
            {
              type: "text",
              text: JSON.stringify(output),
            },
          ],
          isError: false,
        });
      } catch (toolErr) {
        sendResponse(msg.id, {
          content: [
            {
              type: "text",
              text: JSON.stringify({ error: toolErr.message }),
            },
          ],
          isError: true,
        });
      }
    } else {
      sendError(msg.id, -32601, `Method not found: ${msg.method}`);
    }
  } catch (err) {
    sendError(msg.id, -32603, err.message || "Internal error");
  }
});
