// Dials in to Dino on 127.0.0.1 and relays CDP over chrome.debugger, for tabs
// Dino opened itself and tabs the person sent it. RFC-0022 section 6.

const VERSION = chrome.runtime.getManifest().version;
let socket = null;
let retry = 1000;

// Tab ids Dino may drive. Kept in session storage, since the service worker is
// stopped and restarted under us.
async function allowed() {
  const { tabs = [] } = await chrome.storage.session.get("tabs");
  return new Set(tabs);
}
async function allow(tab, yes) {
  const tabs = await allowed();
  yes ? tabs.add(tab) : tabs.delete(tab);
  await chrome.storage.session.set({ tabs: [...tabs] });
}

async function settings() {
  const { port = 8793, token = "" } = await chrome.storage.local.get(["port", "token"]);
  return { port, token };
}

function send(message) {
  if (socket && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
}

async function attach(tab) {
  try {
    await chrome.debugger.attach({ tabId: tab }, "1.3");
  } catch (error) {
    if (!String(error.message).includes("already attached")) throw error;
  }
  // A background tab is throttled; these keep its timers and rendering honest.
  for (const [method, params] of [["Page.setWebLifecycleState", { state: "active" }], ["Emulation.setFocusEmulationEnabled", { enabled: true }]]) {
    await chrome.debugger.sendCommand({ tabId: tab }, method, params).catch(() => {});
  }
}

async function group(tab) {
  const { group: existing } = await chrome.storage.session.get("group");
  try {
    const id = await chrome.tabs.group(existing ? { tabIds: tab, groupId: existing } : { tabIds: tab });
    if (!existing) {
      await chrome.tabGroups.update(id, { title: "Dino Code", color: "grey", collapsed: true });
      await chrome.storage.session.set({ group: id });
    }
  } catch {
    await chrome.storage.session.remove("group");
  }
}

const ops = {
  async hello() {
    return { version: VERSION, tabs: [...(await allowed())] };
  },
  async open() {
    const tab = await chrome.tabs.create({ url: "about:blank", active: false });
    await allow(tab.id, true);
    await group(tab.id);
    await attach(tab.id);
    return { tab: tab.id };
  },
  async cdp({ tab, method, params }) {
    if (!(await allowed()).has(tab)) throw new Error(`tab ${tab} is not one Dino opened or was sent`);
    try {
      return await chrome.debugger.sendCommand({ tabId: tab }, method, params || {});
    } catch (error) {
      if (!String(error.message).includes("not attached")) throw error;
      await attach(tab);
      return await chrome.debugger.sendCommand({ tabId: tab }, method, params || {});
    }
  },
  async close({ tab }) {
    const owned = await allowed();
    if (!owned.has(tab)) return {};
    await allow(tab, false);
    await chrome.debugger.detach({ tabId: tab }).catch(() => {});
    // A tab the person sent is theirs: Dino lets go of it but leaves it open.
    const { shared = [] } = await chrome.storage.session.get("shared");
    if (!shared.includes(tab)) await chrome.tabs.remove(tab).catch(() => {});
    return {};
  },
};

async function connect() {
  if (socket && socket.readyState <= WebSocket.OPEN) return;
  const { port, token } = await settings();
  if (!token) return;
  const ws = new WebSocket(`ws://127.0.0.1:${port}/?token=${encodeURIComponent(token)}`);
  socket = ws;
  ws.onopen = () => { retry = 1000; chrome.action.setBadgeText({ text: "on" }); chrome.action.setBadgeBackgroundColor({ color: "#2e7d32" }); };
  ws.onclose = () => {
    if (socket === ws) socket = null;
    chrome.action.setBadgeText({ text: "" });
    setTimeout(connect, retry);
    retry = Math.min(retry * 2, 30000);
  };
  ws.onmessage = async (event) => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    const { id, op } = message;
    if (!ops[op]) return send({ id, error: { message: `unknown op ${op}` } });
    try {
      send({ id, result: (await ops[op](message)) ?? {} });
    } catch (error) {
      send({ id, error: { message: String(error && error.message ? error.message : error) } });
    }
  };
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (source.tabId !== undefined) send({ op: "event", tab: source.tabId, method, params });
});

// The person pressed "Cancel" on the debugging bar, or closed the tab: Dino lets go.
chrome.debugger.onDetach.addListener(async (source, reason) => {
  if (reason === "canceled_by_user") await allow(source.tabId, false);
});
chrome.tabs.onRemoved.addListener((tab) => allow(tab, false));

chrome.runtime.onMessage.addListener((message, _sender, reply) => {
  (async () => {
    if (message.what === "status") {
      reply({ connected: !!socket && socket.readyState === WebSocket.OPEN, ...(await settings()), tabs: (await allowed()).size });
    } else if (message.what === "save") {
      await chrome.storage.local.set({ port: Number(message.port) || 8793, token: message.token.trim() });
      reply({ ok: true });
    } else if (message.what === "share") {
      const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
      if (!tab || !/^https?:/.test(tab.url || "")) return reply({ ok: false, why: "only web pages can be sent" });
      if (!socket || socket.readyState !== WebSocket.OPEN) return reply({ ok: false, why: "Dino is not listening" });
      await attach(tab.id);
      await allow(tab.id, true);
      const { shared = [] } = await chrome.storage.session.get("shared");
      await chrome.storage.session.set({ shared: [...shared, tab.id] });
      send({ op: "shared", tab: tab.id, url: tab.url, title: tab.title });
      reply({ ok: true });
    }
  })();
  return true;
});

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && (changes.token || changes.port)) {
    if (socket) socket.close();
    else connect();
  }
});

chrome.alarms.create("dino-keepalive", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
