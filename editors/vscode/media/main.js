// Webview-side script. Runs in a restricted context with no Node access;
// all agent communication goes through the extension host via postMessage.
(function () {
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input-box');
  const sendBtn = document.getElementById('send-btn');
  const stopBtn = document.getElementById('stop-btn');
  const modelPill = document.getElementById('model-pill');
  const providerPill = document.getElementById('provider-pill');
  const effortPill = document.getElementById('effort-pill');

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
    el.className = 'tool-call ' + (status || '');
    el.textContent = `${statusIcon(status)} ${title || '(tool call)'}`;
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
  modelPill.addEventListener('click', () => vscode.postMessage({ type: 'pickModel' }));
  providerPill.addEventListener('click', () => vscode.postMessage({ type: 'pickProvider' }));
  effortPill.addEventListener('click', () => vscode.postMessage({ type: 'pickEffort' }));

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
      case 'userEcho': {
        currentAgentBubble = null;
        appendRow(msg.text, 'user');
        setBusy(true);
        break;
      }
      case 'status': {
        currentAgentBubble = null;
        appendRow(msg.text, 'system');
        break;
      }
      case 'headerUpdate': {
        if (msg.model) modelPill.textContent = 'model: ' + msg.model;
        if (msg.provider) providerPill.textContent = 'provider: ' + msg.provider;
        if (msg.effort) effortPill.textContent = 'effort: ' + msg.effort;
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
