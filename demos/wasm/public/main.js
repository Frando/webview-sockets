// UI for the Rust native UDP demo. All networking happens in Rust
// (../src/app.rs); this file only wires the page to the wasm exports.
//
// Query params:
//   ?peer=ip:port&send=text&expect=text  send, wait for the reply, report
//   ?peer=ip:port&gso=4                  one GSO send of 4 datagrams, wait for all echoes
//   ?bench=ip:port[&n=2000&count=20000&window=64&gso=1]  RTT + throughput vs an echo server
//   ?echo=1                              start in echo mode
// Results: console line and document.title "UDP-DEMO PASS|FAIL <detail>".
import initWasm, { start } from "./wasm/webview_sockets_demo_wasm.js";

const HOST_JS = "/.well-known/webview-sockets/polyfill.js";
const MAX_LOG_LINES = 500;

const $ = (id) => document.getElementById(id);
const params = new URLSearchParams(location.search);

function log(line) {
  const element = $("log");
  const lines = element.textContent.split("\n");
  if (lines.length > MAX_LOG_LINES) element.textContent = lines.slice(-MAX_LOG_LINES).join("\n");
  element.textContent += `${new Date().toLocaleTimeString()} ${line}\n`;
  element.scrollTop = element.scrollHeight;
}

function setStatus(text, kind) {
  const element = $("status");
  element.textContent = text;
  element.className = `status ${kind}`;
}

// line is "PASS ..." or "FAIL ...".
function result(line) {
  const full = `UDP-DEMO ${line}`;
  document.title = full;
  console.log(full);
  log(full);
}

function addrRow(label, value) {
  const item = document.createElement("li");
  const labelElement = document.createElement("span");
  labelElement.className = "label";
  labelElement.textContent = label;
  const valueElement = document.createElement("code");
  valueElement.textContent = value;
  const copy = document.createElement("button");
  copy.className = "small";
  copy.textContent = "copy";
  copy.onclick = () => navigator.clipboard?.writeText(value).catch(() => {});
  item.append(labelElement, valueElement, copy);
  $("addrs").append(item);
}

// Outside a webview-sockets host app, try the test-host dev script.
async function loadHostScript() {
  if (window.webviewSockets) return;
  await new Promise((resolve) => {
    const script = document.createElement("script");
    script.src = HOST_JS;
    script.onload = resolve;
    script.onerror = resolve;
    document.head.append(script);
  });
}

const numberParam = (name, fallback) => Number(params.get(name) || fallback);

async function main() {
  $("echo").checked = params.get("echo") === "1";
  if (params.get("peer")) $("peer").value = params.get("peer");
  if (params.get("send")) $("text").value = params.get("send");

  await loadHostScript();
  await initWasm();
  let demo;
  try {
    demo = await start();
  } catch (err) {
    const reason = err.message ?? String(err);
    setStatus(`Native UDP: not available (${reason})`, "bad");
    log(`not available: ${reason}`);
    if (params.has("peer") || params.has("bench")) result(`FAIL not available: ${reason}`);
    return;
  }

  demo.set_on_log(log);
  demo.set_on_close((reason) => {
    setStatus(`Native UDP: ${reason}`, "bad");
    $("send").disabled = true;
    $("send-gso").disabled = true;
  });
  demo.set_echo($("echo").checked);
  $("echo").onchange = () => demo.set_echo($("echo").checked);

  setStatus("Native UDP: available (Rust)", "ok");
  addrRow("bound", demo.local_addr);
  for (const addr of demo.interface_addrs) addrRow("interface", addr);
  log(`bound ${demo.local_addr}, gso=${demo.max_gso_segments} gro=${demo.gro_segments}`);

  const guard = (action) => {
    try {
      action();
    } catch (err) {
      log(`send failed: ${err.message}`);
    }
  };
  $("send").disabled = false;
  $("send-gso").disabled = false;
  $("send-form").onsubmit = (event) => {
    event.preventDefault();
    guard(() => demo.send_text($("peer").value, $("text").value));
  };
  $("send-gso").onclick = () => guard(() => demo.send_gso($("peer").value, $("text").value, 4));

  if (params.get("bench")) {
    $("send").disabled = true;
    const line = await demo.bench(params.get("bench"), numberParam("n", 2000), numberParam("count", 20000),
      numberParam("window", 64), params.get("gso") === "1");
    $("send").disabled = false;
    result(line);
  } else if (params.get("peer") && params.has("gso")) {
    result(await demo.gso_expect(params.get("peer"), params.get("send") || "gso", numberParam("gso", 4)));
  } else if (params.get("peer") && params.has("send")) {
    result(await demo.send_expect(params.get("peer"), params.get("send"), params.get("expect")));
  }
}

main().catch((err) => {
  setStatus(`error: ${err.message}`, "bad");
  result(`FAIL error: ${err.message}`);
});
