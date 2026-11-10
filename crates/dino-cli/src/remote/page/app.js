'use strict';

// Everything from the session is put on the page as text, never as markup:
// a model's answer and a tool's output can contain anything, including what
// someone else wrote into a file this session read.

const log = document.getElementById('log');
const approvals = document.getElementById('approvals');
const statusLine = document.getElementById('status');
const infoLine = document.getElementById('info');
const form = document.getElementById('form');
const text = document.getElementById('text');
const cancel = document.getElementById('cancel');
const sessionsPanel = document.getElementById('sessions');
const workspacesPanel = document.getElementById('workspaces');
const tracePanel = document.getElementById('trace');
const viewButtons = document.querySelectorAll('#views button');

// The token arrives in the address fragment, which the browser never sends
// anywhere. It is kept for this tab only and taken out of the address bar.
let token = sessionStorage.getItem('dino-token');
if (location.hash.length > 1) {
  token = decodeURIComponent(location.hash.slice(1));
  sessionStorage.setItem('dino-token', token);
  history.replaceState(null, '', location.pathname);
}

let last = 0;
let answer = null; // the block a streamed answer is being added to
const cards = new Map();
let busy = false;
let info = null;
let view = 'chat';

function show(kind, content) {
  const nearBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 80;
  const line = document.createElement('div');
  line.className = kind;
  line.textContent = content;
  log.append(line);
  if (nearBottom) log.scrollTop = log.scrollHeight;
  return line;
}

function setStatus(label, kind) {
  statusLine.textContent = label;
  statusLine.className = kind || '';
}

async function get(path) {
  const response = await fetch(path, { headers: { Authorization: 'Bearer ' + token } });
  if (!response.ok) throw new Error(response.status + ' ' + (await response.text()));
  return response.json();
}

async function api(path, body) {
  const response = await fetch(path, {
    method: 'POST',
    headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error(response.status + ' ' + (await response.text()));
  return response.json();
}

function summarise(call) {
  const args = call.arguments || {};
  const detail = args.path || args.command || args.query || args.op || '';
  return call.name + (detail ? ' ' + (Array.isArray(detail) ? detail.join(' ') : detail) : '');
}

function onEvent(event) {
  switch (event.type) {
    case 'submitted':
      answer = null;
      show('you', event.text);
      break;
    case 'model': {
      const inner = event.event;
      if (inner.type === 'text_delta') {
        if (!answer) answer = show('', '');
        answer.textContent += inner.text;
        log.scrollTop = log.scrollHeight;
      } else if (inner.type === 'tool_call_completed') {
        answer = null;
        show('tool', '→ ' + summarise(inner.call));
      } else if (inner.type === 'searched') {
        answer = null;
        const how = inner.error ? 'failed: ' + inner.error : inner.results.length + ' results';
        show('tool', '✱ web search "' + inner.query + '" · ' + how + ' (at the provider)');
      } else if (inner.type === 'finished') {
        answer = null;
      }
      break;
    }
    case 'tool_decided':
      if (event.decision.outcome === 'denied') show('err', 'refused ' + event.tool + ': ' + event.decision.reason);
      break;
    case 'tool_finished': {
      const out = event.result.output;
      show(out.is_error ? 'err' : 'out', out.content);
      break;
    }
    case 'steered':
      show('you', event.text);
      break;
    case 'noted':
    case 'sent_back':
    case 'turn_retried':
    case 'turns_running_out':
    case 'context_warning':
    case 'handed_over':
    case 'elided':
      show('note', event.type.replaceAll('_', ' ') + (event.reason ? ': ' + event.reason : event.note ? ': ' + event.note : ''));
      break;
    default:
      break;
  }
}

function onApproval(approval) {
  if (cards.has(approval.id)) return;
  const card = document.createElement('div');
  card.className = 'approval';
  const title = document.createElement('div');
  title.textContent = approval.summary + '  (' + approval.class + ')';
  card.append(title);
  if (approval.unconfined) {
    const warn = document.createElement('div');
    warn.className = 'warn';
    warn.textContent = 'asks to run OUTSIDE the sandbox (full network, your credentials, every file): ' + approval.unconfined;
    card.append(warn);
  }
  const args = document.createElement('pre');
  args.textContent = approval.tool + ' on ' + approval.subject + '\n' + approval.arguments;
  card.append(args);
  const choices = document.createElement('div');
  choices.className = 'choices';
  for (const choice of approval.choices) {
    const button = document.createElement('button');
    button.textContent = choice.label;
    if (choice.key === 'reject') button.className = 'reject';
    button.addEventListener('click', async () => {
      for (const other of choices.children) other.disabled = true;
      try {
        await api('/api/approve', { id: approval.id, choice: choice.key });
      } catch (error) {
        show('err', 'could not answer: ' + error.message);
        for (const other of choices.children) other.disabled = false;
      }
    });
    choices.append(button);
  }
  card.append(choices);
  approvals.append(card);
  cards.set(approval.id, card);
}

// The model's questions: a button per option, or boxes to tick where several
// may apply, and words of the person's own. The first option is recommended.
function onQuestion(card) {
  if (cards.has(card.id)) return;
  const box = document.createElement('div');
  box.className = 'approval';
  const picks = card.questions.map(() => new Set());
  const others = [];
  const send = document.createElement('button');
  send.textContent = 'send';
  send.className = 'primary';
  card.questions.forEach((question, q) => {
    const title = document.createElement('div');
    title.className = 'question';
    title.textContent = question.question + (question.multi_select ? '  (any that apply)' : '');
    box.append(title);
    const choices = document.createElement('div');
    choices.className = 'choices';
    question.options.forEach((option, index) => {
      const button = document.createElement('button');
      button.textContent = option.label + (index === 0 ? ' (recommended)' : '');
      if (option.description) button.title = option.description;
      button.addEventListener('click', () => {
        if (!question.multi_select) {
          picks[q].clear();
          for (const other of choices.children) other.classList.remove('on');
        }
        if (picks[q].has(index)) {
          picks[q].delete(index);
          button.classList.remove('on');
        } else {
          picks[q].add(index);
          button.classList.add('on');
        }
      });
      choices.append(button);
    });
    box.append(choices);
    const other = document.createElement('input');
    other.placeholder = 'or say something else';
    others.push(other);
    box.append(other);
  });
  send.addEventListener('click', async () => {
    send.disabled = true;
    const answers = card.questions.map((_, q) => ({
      chosen: [...picks[q]].sort((a, b) => a - b),
      other: others[q].value.trim() || null,
    }));
    try {
      await api('/api/answer', { id: card.id, answers });
    } catch (error) {
      show('err', 'could not answer: ' + error.message);
      send.disabled = false;
    }
  });
  const row = document.createElement('div');
  row.className = 'choices';
  row.append(send);
  box.append(row);
  approvals.append(box);
  cards.set(card.id, box);
}

function onFrame(frame) {
  last = Math.max(last, frame.seq);
  switch (frame.kind) {
    case 'event':
      onEvent(frame.event);
      break;
    case 'approval':
      onApproval(frame.approval);
      break;
    case 'question':
      onQuestion(frame.question);
      break;
    case 'resolved': {
      const card = cards.get(frame.id);
      if (card) card.remove();
      cards.delete(frame.id);
      if (frame.outcome === 'withdrawn') show('note', 'a question was withdrawn');
      break;
    }
    case 'state': {
      const moved = frame.info && (!info || frame.info.workspace !== info.workspace || frame.info.conversation !== info.conversation);
      if (frame.info) {
        info = frame.info;
        infoLine.textContent = [info.model, info.workspace, info.sandbox, info.approvals].join(' · ');
      }
      busy = frame.busy;
      setStatus(busy ? 'working' : 'idle', busy ? 'busy' : '');
      cancel.disabled = !busy;
      if (moved || view === 'sessions' || view === 'workspaces') refreshView();
      break;
    }
    case 'notice':
      show('note', frame.text);
      break;
    default:
      break;
  }
}

function onChunk(chunk) {
  for (const line of chunk.split('\n')) {
    if (!line.startsWith('data: ')) continue;
    try {
      onFrame(JSON.parse(line.slice(6)));
    } catch (error) {
      show('err', 'unreadable frame: ' + error.message);
    }
  }
}

async function connect() {
  if (!token) {
    setStatus('open the address dino printed, with its #token', 'bad');
    return;
  }
  for (;;) {
    try {
      const response = await fetch('/api/events?after=' + last, { headers: { Authorization: 'Bearer ' + token } });
      if (response.status === 401) {
        sessionStorage.removeItem('dino-token');
        setStatus('the token was refused', 'bad');
        return;
      }
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let end;
        while ((end = buffer.indexOf('\n\n')) >= 0) {
          onChunk(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
        }
      }
    } catch (error) {
      // fall through to reconnect
    }
    setStatus('reconnecting', 'bad');
    await new Promise((resolve) => setTimeout(resolve, 2000));
  }
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const message = text.value.trim();
  if (!message) return;
  try {
    const result = await api('/api/prompt', { text: message });
    text.value = '';
    if (result.queued) show('note', 'queued: for the model with its next tool results, or after this run');
  } catch (error) {
    show('err', 'not sent: ' + error.message);
  }
});

cancel.addEventListener('click', async () => {
  try {
    await api('/api/cancel', {});
  } catch (error) {
    show('err', 'could not stop: ' + error.message);
  }
});

// Sessions, workspaces and a session's trajectory. Each list comes from the
// server, and a page can only pick from it: a resume or a move names an entry,
// which the server checks against the same list.

function el(tag, className, content) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content !== undefined) node.textContent = content;
  return node;
}

function button(label, action, disabled) {
  const node = el('button', '', label);
  node.type = 'button';
  node.disabled = !!disabled;
  node.addEventListener('click', action);
  return node;
}

function took(ms) {
  if (ms === null || ms === undefined) return '?';
  if (ms < 1000) return ms + 'ms';
  if (ms < 60000) return (ms / 1000).toFixed(1) + 's';
  if (ms < 3600000) return Math.floor(ms / 60000) + 'm ' + String(Math.floor(ms / 1000) % 60).padStart(2, '0') + 's';
  return Math.floor(ms / 3600000) + 'h ' + String(Math.floor(ms / 60000) % 60).padStart(2, '0') + 'm';
}

function age(seconds) {
  if (seconds < 60) return seconds + 's';
  if (seconds < 3600) return Math.floor(seconds / 60) + 'm';
  if (seconds < 86400) return Math.floor(seconds / 3600) + 'h';
  return Math.floor(seconds / 86400) + 'd';
}

function showView(name) {
  view = name;
  for (const b of viewButtons) b.classList.toggle('on', b.dataset.view === name);
  log.hidden = name !== 'chat';
  form.hidden = name !== 'chat';
  sessionsPanel.hidden = name !== 'sessions';
  workspacesPanel.hidden = name !== 'workspaces';
  tracePanel.hidden = name !== 'trace';
  refreshView();
}

function refreshView() {
  if (view === 'sessions') loadSessions();
  if (view === 'workspaces') loadWorkspaces();
}

async function loadSessions() {
  let listed;
  try {
    listed = (await get('/api/sessions')).sessions;
  } catch (error) {
    sessionsPanel.replaceChildren(el('div', 'err', 'could not list sessions: ' + error.message));
    return;
  }
  const rows = [];
  if (info && info.log) {
    rows.push(el('div', 'hint', 'this run is ' + info.log));
    rows.push(button('trace this run', () => openTrace(info.log)));
  }
  if (busy) rows.push(el('div', 'hint', 'a run is under way; resume when it is idle'));
  for (const session of listed) {
    const row = el('div', 'row-item');
    const current = info && session.id === info.conversation;
    const label = session.name || session.first_prompt || session.id;
    row.append(el('span', 'what' + (current ? ' current' : ''), label + (current ? '  (current)' : '')));
    const meta = [age(session.age_seconds) + ' ago', session.model || '?'];
    meta.push(session.submissions === 1 ? '1 prompt' : session.submissions + ' prompts');
    if (session.name || session.first_prompt) meta.push(session.id);
    row.append(el('span', 'meta', meta.join(' · ')));
    if (session.traceable) row.append(button('trace', () => openTrace(session.id)));
    if (session.resumable && !current) {
      row.append(button('resume', async (event) => {
        event.target.disabled = true;
        try {
          await api('/api/resume', { id: session.id });
          showView('chat');
        } catch (error) {
          show('err', 'could not resume: ' + error.message);
          event.target.disabled = false;
        }
      }, busy));
    }
    rows.push(row);
  }
  if (!listed.length) rows.push(el('div', 'hint', 'no sessions saved in this workspace'));
  sessionsPanel.replaceChildren(...rows);
}

async function loadWorkspaces() {
  let offered;
  try {
    offered = (await get('/api/workspaces')).workspaces;
  } catch (error) {
    workspacesPanel.replaceChildren(el('div', 'err', 'could not list workspaces: ' + error.message));
    return;
  }
  const rows = [el('div', 'hint', 'The workspaces listed in your config file, and those used lately. Moving ends this session and starts one there, with the same flags.')];
  for (const place of offered) {
    const row = el('div', 'row-item');
    row.append(el('span', 'what' + (place.current ? ' current' : ''), place.path));
    const tags = [place.current && 'current', place.configured && 'configured', place.recent && 'recent'].filter(Boolean);
    row.append(el('span', 'meta', tags.join(' · ')));
    if (!place.current) {
      row.append(button('move here', async (event) => {
        if (!window.confirm('End this session and start one in ' + place.path + '?')) return;
        event.target.disabled = true;
        try {
          await api('/api/workspace', { path: place.path });
          showView('chat');
        } catch (error) {
          show('err', 'could not move: ' + error.message);
          event.target.disabled = false;
        }
      }, busy));
    }
    rows.push(row);
  }
  workspacesPanel.replaceChildren(...rows);
}

function traceCall(call) {
  const node = el('details', 'call' + (call.is_error || call.refusal ? ' bad' : ''));
  const summary = el('summary', '', call.summary + '  ');
  const timing = [];
  if (call.decision === 'user') timing.push('approved after ' + took(call.decided_ms));
  if (call.refusal) timing.push('refused after ' + took(call.decided_ms));
  if (call.run_ms !== null && call.run_ms !== undefined) timing.push('ran ' + took(call.run_ms));
  if (call.gist) timing.push(call.gist);
  summary.append(el('span', 'took', timing.join(' · ')));
  node.append(summary);
  if (call.refusal) node.append(el('div', 'err', call.refusal));
  node.append(el('pre', '', call.arguments));
  if (call.output) node.append(el('pre', '', call.output));
  return node;
}

function summaryLine(totals, wall) {
  const parts = [wall === null || wall === undefined ? 'unfinished' : took(wall), totals.turns + ' turn(s)', totals.calls + ' call(s)', 'model ' + took(totals.model_ms), 'tools ' + took(totals.tools_ms)];
  if (totals.approvals_ms) parts.push('approvals ' + took(totals.approvals_ms));
  return parts.join(' · ');
}

async function openTrace(id) {
  showView('trace');
  tracePanel.replaceChildren(el('div', 'hint', 'reading ' + id + '…'));
  let trajectory;
  try {
    trajectory = await get('/api/trace?id=' + encodeURIComponent(id));
  } catch (error) {
    tracePanel.replaceChildren(el('div', 'err', 'could not read ' + id + ': ' + error.message));
    return;
  }
  const rows = [button('back to sessions', () => showView('sessions')), el('div', 'hint', id + ' · ' + summaryLine(trajectory.totals, trajectory.totals.wall_ms))];
  for (const submission of trajectory.submissions) {
    const block = el('div', 'sub');
    block.append(el('div', 'asked', '› ' + (submission.text === null ? '(from before submissions were recorded)' : submission.text)));
    block.append(el('div', 'sum', summaryLine(submission.totals, submission.wall_ms) + (submission.stop_reason ? ' · ' + submission.stop_reason : '')));
    if (submission.error) block.append(el('div', 'err', submission.error));
    for (const note of submission.notes) block.append(el('div', 'note', note));
    for (const turn of submission.turns) {
      const node = el('div', 'turn');
      const head = ['turn ' + turn.number, 'model ' + (turn.model_ms === null ? 'unfinished' : took(turn.model_ms))];
      if (turn.first_response_ms !== null) head.push('first response ' + took(turn.first_response_ms));
      head.push(turn.usage.input_tokens + ' in', turn.usage.output_tokens + ' out');
      if (turn.stop) head.push(turn.stop);
      node.append(el('div', 'head', head.join(' · ')));
      if (turn.text.trim()) node.append(el('div', 'said', turn.text.trim()));
      for (const call of turn.calls) node.append(traceCall(call));
      for (const note of turn.notes) node.append(el('div', 'note', note));
      block.append(node);
    }
    rows.push(block);
  }
  if (trajectory.skipped) rows.push(el('div', 'hint', trajectory.skipped + ' line(s) of the log not read'));
  tracePanel.replaceChildren(...rows);
}

for (const b of viewButtons) b.addEventListener('click', () => showView(b.dataset.view));

connect();
