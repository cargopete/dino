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
    warn.textContent = 'asks to run OUTSIDE the sandbox: ' + approval.unconfined + '. Refuse here; approving needs the terminal.';
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

function onFrame(frame) {
  last = Math.max(last, frame.seq);
  switch (frame.kind) {
    case 'event':
      onEvent(frame.event);
      break;
    case 'approval':
      onApproval(frame.approval);
      break;
    case 'resolved': {
      const card = cards.get(frame.id);
      if (card) card.remove();
      cards.delete(frame.id);
      if (frame.outcome === 'withdrawn') show('note', 'a question was withdrawn');
      break;
    }
    case 'state':
      if (frame.info) infoLine.textContent = [frame.info.model, frame.info.workspace, frame.info.sandbox, frame.info.approvals].join(' · ');
      setStatus(frame.busy ? 'working' : 'idle', frame.busy ? 'busy' : '');
      cancel.disabled = !frame.busy;
      break;
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

connect();
