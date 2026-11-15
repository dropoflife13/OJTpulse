// Zero-dependency test script to simulate a live coding agent with two-way blocking permission control
const ENDPOINT = "http://127.0.0.1:4747/agent";
const SESSION_ID = "test-agent-1";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function sendUpdate(payload) {
  const body = {
    id: SESSION_ID,
    name: "Cursor Agent",
    workspace_path: "c:\\Users\\neil\\dinobuds",
    updated_at: Math.floor(Date.now() / 1000),
    ...payload,
  };

  const res = await fetch(ENDPOINT, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

  if (!res.ok) {
    throw new Error(`HTTP ${res.status}: ${await res.text()}`);
  }
  console.log(
    `[DinoBuds Test] Sent -> [${body.status}] (${body.current_step}/${body.total_steps} ${body.step_label}): ${body.task}`
  );
}

async function waitForUserDecision(id) {
  while (true) {
    await sleep(500);
    const res = await fetch(`${ENDPOINT}/${encodeURIComponent(id)}`);
    if (!res.ok) {
      throw new Error(`Failed polling session ${id}: HTTP ${res.status}`);
    }
    const session = await res.json();
    if (session.decision === true || session.decision === false) {
      return session.decision;
    }
  }
}

async function runSimulation() {
  console.log("🦖 Starting DinoBuds two-way telemetry simulation...\n");

  await sendUpdate({
    status: "queued",
    current_step: 0,
    total_steps: 5,
    step_label: "Queued",
    task: "Preparing workspace & indexing files...",
    question: null,
  });
  await sleep(1500);

  await sendUpdate({
    status: "running",
    current_step: 1,
    total_steps: 5,
    step_label: "Indexing",
    task: "Scanning Rust Cargo.toml and Tauri capabilities",
    question: null,
  });
  await sleep(2000);

  await sendUpdate({
    status: "running",
    current_step: 2,
    total_steps: 5,
    step_label: "Refactoring",
    task: "Wiring two-way decision endpoint in src-tauri/src/lib.rs",
    question: null,
  });
  await sleep(2000);

  await sendUpdate({
    status: "waiting_permission",
    current_step: 3,
    total_steps: 5,
    step_label: "Permission",
    task: "Waiting for user approval in DinoBuds HUD",
    question: "Allow command: cargo test --manifest-path src-tauri/Cargo.toml?",
  });
  console.log(
    `⏸️  Waiting for your click on the DinoBuds HUD (polling GET ${ENDPOINT}/${SESSION_ID} every 500ms)...`
  );

  const approved = await waitForUserDecision(SESSION_ID);

  if (!approved) {
    console.log("🛑 Aborted by user in DinoBuds");
    process.exit(0);
  }

  console.log("✅ Approved in DinoBuds! Resuming step 4 & 5...\n");

  await sendUpdate({
    status: "running",
    current_step: 4,
    total_steps: 5,
    step_label: "Verifying",
    task: "Running integration tests & bundling HUD assets",
    question: null,
  });
  await sleep(2000);

  await sendUpdate({
    status: "done",
    current_step: 5,
    total_steps: 5,
    step_label: "Completed",
    task: "All 5 steps finished! Two-way HUD control verified.",
    question: null,
  });

  console.log("\n🎉 Simulation complete!");
}

runSimulation().catch((err) => {
  console.error(
    "❌ Failed to reach DinoBuds telemetry server at 127.0.0.1:4747:",
    err.message
  );
  process.exit(1);
});
