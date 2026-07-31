// Webview-side script. Runs in a restricted context with no Node access;
// all agent communication goes through the extension host via postMessage.
(function () {
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input-box');
  const sendBtn = document.getElementById('send-btn');
  const stopBtn = document.getElementById('stop-btn');

  let currentAgentBubble = null;

  function appendMessage(text, cls) {
    const el = document.createElement('div');
    el.className = 'message ' + cls;
    el.textContent = text;
    messagesEl.appendChild(el);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return el;
  }

  function appendToolCall(title, status) {
    const el = document.createElement('div');
    el.className = 'tool-call ' + (status || '');
    el.textContent = title || '(tool call)';
    el.dataset.title = title || '';
    messagesEl.appendChild(el);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return el;
  }

  function send() {
    const text = inputEl.value.trim();
    if (!text) {
      return;
    }
    appendMessage(text, 'user');
    inputEl.value = '';
    currentAgentBubble = null;
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

  window.addEventListener('message', (event) => {
    const msg = event.data;
    switch (msg.type) {
      case 'textChunk': {
        if (!currentAgentBubble || currentAgentBubble.dataset.isThought !== String(msg.isThought)) {
          currentAgentBubble = appendMessage('', msg.isThought ? 'thought' : 'agent');
          currentAgentBubble.dataset.isThought = String(msg.isThought);
        }
        currentAgentBubble.textContent += msg.text;
        messagesEl.scrollTop = messagesEl.scrollHeight;
        break;
      }
      case 'toolCall': {
        currentAgentBubble = null;
        appendToolCall(msg.title, msg.status);
        break;
      }
      case 'toolCallUpdate': {
        currentAgentBubble = null;
        appendToolCall(msg.title, msg.status);
        break;
      }
      case 'status': {
        currentAgentBubble = null;
        appendMessage(msg.text, 'thought');
        break;
      }
      case 'turnEnded': {
        currentAgentBubble = null;
        break;
      }
      default:
        break;
    }
  });
})();
