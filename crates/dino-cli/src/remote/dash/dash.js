"use strict";
const token = location.hash.slice(1);
const cards = document.getElementById("cards");
const summary = document.getElementById("summary");

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}
function took(ms) {
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + "s";
  if (s < 3600) return Math.floor(s / 60) + "m " + (s % 60) + "s";
  return Math.floor(s / 3600) + "h " + Math.floor((s % 3600) / 60) + "m";
}
function tokens(n) { return n >= 1000 ? (n / 1000).toFixed(1) + "k" : String(n); }
const LABEL = { stuck: "stuck", needs_you: "needs you", working: "working", idle: "idle", done: "done", failed: "failed", gone: "dead" };

function card(s, now) {
  const root = el("div", "card " + s.state);
  const top = el("div", "top");
  top.append(el("span", "pill", LABEL[s.state] || s.state));
  const base = s.workspace.split("/").filter(Boolean).pop() || s.workspace;
  const where = el("span", "where", base);
  where.title = s.workspace;
  top.append(where, el("span", "age", took(Math.max(0, now - s.last_ms)) + " ago"));
  root.append(top);
  root.append(el("div", "doing", s.doing + (s.for_ms > 5000 ? " (" + took(s.for_ms) + ")" : "")));
  for (const p of s.problems) root.append(el("div", "problem", "! " + p));
  if (s.task) root.append(el("div", "task", "› " + s.task));
  if (s.max_turns > 0) {
    const meter = el("div", "meter");
    const bar = el("div");
    bar.style.width = Math.min(100, (100 * s.turn) / s.max_turns) + "%";
    meter.append(bar);
    root.append(meter);
  }
  const bits = [s.model, "turn " + s.turn + "/" + s.max_turns, s.calls + " calls",
    tokens(s.input_tokens) + " in / " + tokens(s.output_tokens) + " out",
    "up " + took(now - s.started_ms), s.pid ? "pid " + s.pid : "no pid", s.id];
  root.append(el("div", "facts", bits.join(" · ")));
  if (s.steps.length) {
    const list = el("ul", "steps");
    for (const step of s.steps.slice().reverse()) {
      const li = el("li", step.error ? "err" : "", step.summary + (step.ms != null ? "  " + took(step.ms) : ""));
      list.append(li);
    }
    root.append(list);
  }
  for (const n of s.notes) root.append(el("div", "notes", n));
  return root;
}

async function tick() {
  try {
    const response = await fetch("/api/sessions", { headers: { Authorization: "Bearer " + token } });
    if (!response.ok) throw new Error(response.status === 401 ? "wrong or missing token" : "HTTP " + response.status);
    const data = await response.json();
    const count = (st) => data.sessions.filter((s) => st.includes(s.state)).length;
    const live = count(["working", "needs_you", "stuck", "idle"]);
    const trouble = count(["stuck", "gone", "failed"]);
    summary.textContent = live + " live, " + count(["working"]) + " working, " + count(["needs_you"]) + " need you, " + trouble + " stuck/dead/failed";
    summary.className = trouble ? "bad" : "";
    document.title = (trouble ? "(" + trouble + "!) " : "") + "dino sessions";
    const blurb = document.getElementById("blurb");
    blurb.hidden = !data.blurb;
    blurb.textContent = data.blurb || "";
    cards.replaceChildren(...(data.sessions.length ? data.sessions.map((s) => card(s, data.now)) : [el("div", "empty", "no sessions in the last 24 hours")]));
  } catch (error) {
    summary.textContent = "cannot reach dino dash: " + error.message;
    summary.className = "bad";
  }
}
tick();
setInterval(tick, 2000);
