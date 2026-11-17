// Opens a URL in headless Chrome and waits until document.title starts with
// a prefix, printing the console log. Exit code 0 if the title contains PASS.
//
//   node run-chrome.mjs <url> [prefix=UDP-TEST] [timeout_ms=20000]
//
// Needs Node >= 22 (global WebSocket) and google-chrome-stable, or set
// CHROME=/path/to/chrome.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [url, prefix = "UDP-TEST", timeoutMs = "20000"] = process.argv.slice(2);
if (!url) {
  console.error("usage: node run-chrome.mjs <url> [prefix] [timeout_ms]");
  process.exit(2);
}
const chrome = process.env.CHROME || "google-chrome-stable";
const profile = mkdtempSync(join(tmpdir(), "udp-chrome-"));
const port = 9300 + Math.floor(Math.random() * 600);
const proc = spawn(chrome, [
  "--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check",
  `--user-data-dir=${profile}`, `--remote-debugging-port=${port}`, "about:blank",
], { stdio: "ignore" });

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let exitCode = 1;
try {
  let target;
  for (let i = 0; i < 100 && !target; i++) {
    await sleep(100);
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      target = list.find((entry) => entry.type === "page");
    } catch {}
  }
  if (!target) throw new Error("chrome did not start");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let id = 0;
  const waiting = new Map();
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && waiting.has(message.id)) {
      waiting.get(message.id)(message);
      waiting.delete(message.id);
    } else if (message.method === "Runtime.consoleAPICalled") {
      const args = message.params.args.map((arg) => arg.value ?? arg.description);
      console.log(`[console.${message.params.type}]`, args.join(" "));
    } else if (message.method === "Runtime.exceptionThrown") {
      const details = message.params.exceptionDetails;
      console.log("[exception]", details.exception?.description ?? details.text);
    }
  };
  const call = (method, params = {}) => new Promise((resolve) => {
    const callId = ++id;
    waiting.set(callId, resolve);
    ws.send(JSON.stringify({ id: callId, method, params }));
  });
  await call("Runtime.enable");
  await call("Page.navigate", { url });
  const deadline = Date.now() + Number(timeoutMs);
  let title = "";
  while (Date.now() < deadline) {
    await sleep(200);
    const reply = await call("Runtime.evaluate", { expression: "document.title" });
    title = reply.result?.result?.value ?? "";
    if (title.startsWith(prefix)) break;
  }
  console.log(`title: ${title}`);
  exitCode = title.startsWith(prefix) && title.includes("PASS") ? 0 : 1;
  ws.close();
} catch (err) {
  console.error(err);
} finally {
  proc.kill();
  await sleep(200);
  rmSync(profile, { recursive: true, force: true });
  process.exit(exitCode);
}
