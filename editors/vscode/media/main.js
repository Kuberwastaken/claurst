// Webview-side script. Runs in a restricted context with no Node access;
// all agent communication goes through the extension host via postMessage.
(function () {
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input-box');
  const sendBtn = document.getElementById('send-btn');
  const stopBtn = document.getElementById('stop-btn');

  let currentAgentBubble = null;
  const toolCallEls = new Map();

  function appendRow(text, cls) {
    const row = document.createElement('div');
    row.className = 'row ' + cls;
    const bubble = document.createElement('div');
    bubble.className = 'bubble ' + cls;
    bubble.textContent = text;
    row.appendChild(bubble);
    messagesEl.appendChild(row);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return bubble;
  }

  function statusIcon(status) {
    if (status === 'completed') return '✓';
    if (status === 'failed') return '✗';
    if (status === 'in_progress' || status === 'pending') return '◌';
    return '•';
  }

  // The initial tool_call event carries a title; the tool_call_update sent
  // on completion never does (the agent only sends status + content there).
  // Remember the title on the element itself so completion doesn't blank it.
  function upsertToolCall(id, title, status) {
    let el = id ? toolCallEls.get(id) : null;
    if (!el) {
      el = document.createElement('div');
      el.className = 'tool-call';
      messagesEl.appendChild(el);
      if (id) {
        toolCallEls.set(id, el);
      }
    }
    if (title) {
      el.dataset.title = title;
    }
    el.className = 'tool-call ' + (status || '');
    el.textContent = `${statusIcon(status)} ${el.dataset.title || '(tool call)'}`;
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function setBusy(busy) {
    sendBtn.disabled = busy;
    stopBtn.classList.toggle('hidden', !busy);
  }

  function autoResize() {
    inputEl.style.height = 'auto';
    inputEl.style.height = Math.min(inputEl.scrollHeight, 200) + 'px';
  }
  inputEl.addEventListener('input', autoResize);

  function send() {
    const text = inputEl.value.trim();
    if (!text) {
      return;
    }
    appendRow(text, 'user');
    inputEl.value = '';
    autoResize();
    currentAgentBubble = null;
    setBusy(true);
    vscode.postMessage({ type: 'prompt', text });
  }

  sendBtn.addEventListener('click', send);
  stopBtn.addEventListener('click', () => vscode.postMessage({ type: 'stop' }));
  inputEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  });

  setBusy(false);

  window.addEventListener('message', (event) => {
    const msg = event.data;
    switch (msg.type) {
      case 'textChunk': {
        const cls = msg.isThought ? 'thought' : 'agent';
        if (!currentAgentBubble || currentAgentBubble.dataset.cls !== cls) {
          currentAgentBubble = appendRow('', cls);
          currentAgentBubble.dataset.cls = cls;
        }
        currentAgentBubble.textContent += msg.text;
        messagesEl.scrollTop = messagesEl.scrollHeight;
        break;
      }
      case 'toolCall':
      case 'toolCallUpdate': {
        currentAgentBubble = null;
        upsertToolCall(msg.toolCallId, msg.title, msg.status);
        break;
      }
      case 'status': {
        currentAgentBubble = null;
        appendRow(msg.text, 'system');
        break;
      }
      case 'turnEnded': {
        currentAgentBubble = null;
        setBusy(false);
        break;
      }
      default:
        break;
    }
  });
})();
