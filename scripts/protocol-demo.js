#!/usr/bin/env node

const { spawn } = require("node:child_process");
const readline = require("node:readline");
const path = require("node:path");

const runtimeBin = process.env.TAKKA_RUNTIME_BIN
  || path.resolve(__dirname, "../target/debug/takka-runtime");
const task = process.argv.slice(2).join(" ").trim();
const autoApprove = process.env.TAKKA_PROTOCOL_AUTO_APPROVE === "1";
const child = spawn(runtimeBin, [], { stdio: ["pipe", "pipe", "inherit"] });
let sequence = 0;
let runStarted = false;

function send(type, payload = {}) {
  sequence += 1;
  child.stdin.write(`${JSON.stringify({ v: 2, id: `node-${sequence}`, type, ...payload })}\n`);
}

const lines = readline.createInterface({ input: child.stdout });
lines.on("line", (line) => {
  const event = JSON.parse(line);
  process.stdout.write(`${line}\n`);

  if (event.type === "ready") {
    send("init", { overrides: {} });
    return;
  }
  if (event.type === "init_ok") {
    if (task) {
      runStarted = true;
      send("run", { task, attachments: [] });
    } else {
      send("shutdown");
      child.stdin.end();
    }
    return;
  }
  if (event.type === "confirmation_request") {
    send("confirm_response", {
      requestId: event.requestId,
      approved: autoApprove,
    });
    return;
  }
  if (event.type === "question_request") {
    const answers = (event.questions || []).map((question) => ({
      question: question.question,
      answer: [],
      skipped: true,
    }));
    send("question_response", { requestId: event.requestId, answers });
    return;
  }
  if (event.type === "done" || (runStarted && event.type === "error")) {
    send("shutdown");
    child.stdin.end();
  }
});

child.on("exit", (code) => {
  process.exitCode = code ?? 1;
});

child.on("error", (error) => {
  console.error(`failed to start ${runtimeBin}: ${error.message}`);
  process.exitCode = 1;
});
