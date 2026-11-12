const pluginId = (location.pathname.split("/")[2] || "realtime");
const rpcId = { sse: 1, ws: 100 };

function append(id, text) {
  const log = document.getElementById(id);
  const stamp = new Date().toISOString().slice(11, 19);
  log.textContent += "[" + stamp + "] " + text + "\n";
  log.scrollTop = log.scrollHeight;
}

function setState(id, text, ok) {
  const el = document.getElementById(id);
  el.textContent = text;
  el.className = "state " + (ok === true ? "ok" : ok === false ? "bad" : "");
}

async function callRpc(name, args) {
  const response = await fetch("rpc", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: rpcId.sse++,
      method: "tools/call",
      params: { name, arguments: args },
    }),
  });
  return response.json();
}

let sse = null;
function connectSse() {
  if (sse) sse.close();
  setState("sse-state", "connecting…");
  sse = new EventSource("events");
  sse.onopen = () => setState("sse-state", "connected", true);
  sse.onerror = () => setState("sse-state", "reconnecting…");
  sse.addEventListener("message", (event) => {
    const data = JSON.parse(event.data);
    append("sse-log", "id=" + event.lastEventId + " " + JSON.stringify(data));
  });
  sse.addEventListener("reset", (event) => {
    append("sse-log", "reset: " + event.data + " — refetch state with tools/call");
  });
}

document.getElementById("sse-call").addEventListener("click", async () => {
  try {
    const result = await callRpc("echo", { text: "hello from SSE + POST" });
    append("sse-log", "rpc → " + JSON.stringify(result));
  } catch (error) {
    append("sse-log", "rpc error: " + error);
  }
});
document.getElementById("sse-reconnect").addEventListener("click", () => {
  append("sse-log", "closing EventSource (Last-Event-ID is replayed on reconnect)");
  sse.close();
  setTimeout(connectSse, 300);
});

let ws = null;
let wsTimer = null;
function connectWs() {
  if (wsTimer) clearTimeout(wsTimer);
  if (ws) ws.close();
  setState("ws-state", "connecting…");
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(scheme + "://" + location.host + "/plugin-ui/" + pluginId + "/ws");
  ws.onopen = () => setState("ws-state", "connected", true);
  ws.onclose = () => {
    setState("ws-state", "closed — retrying in 1s", false);
    wsTimer = setTimeout(connectWs, 1000);
  };
  ws.onerror = () => setState("ws-state", "error", false);
  ws.onmessage = (event) => {
    let message;
    try { message = JSON.parse(event.data); } catch { message = event.data; }
    append("ws-log", JSON.stringify(message));
  };
}

function wsCall() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  ws.send(JSON.stringify({
    jsonrpc: "2.0",
    id: rpcId.ws++,
    method: "tools/call",
    params: { name: "echo", arguments: { text: "hello from WebSocket" } },
  }));
}

document.getElementById("ws-call").addEventListener("click", wsCall);
document.getElementById("ws-reconnect").addEventListener("click", connectWs);

connectSse();
connectWs();
