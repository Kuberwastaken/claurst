(function () {
  var dataEl = document.getElementById('session-data');
  if (!dataEl) return;

  var payload;
  try {
    var raw = atob((dataEl.textContent || '').trim());
    var decoded = decodeURIComponent(Array.prototype.map.call(raw, function (c) {
      return '%' + ('00' + c.charCodeAt(0).toString(16)).slice(-2);
    }).join(''));
    payload = JSON.parse(decoded);
  } catch (e) {
    document.getElementById('messages').innerHTML =
      '<p style="color:#f88">Failed to load session data: ' + escapeHtml(String(e)) + '</p>';
    return;
  }

  var meta = payload.meta || {};
  var messages = payload.messages || [];

  if (window.marked && window.hljs) {
    marked.setOptions({
      highlight: function (code, lang) {
        try {
          if (lang && hljs.getLanguage(lang)) {
            return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value;
          }
          return hljs.highlightAuto(code).value;
        } catch (_) {
          return code;
        }
      },
      breaks: true,
      gfm: true
    });
  }

  var title = meta.title || ('Session ' + (meta.session_id || ''));
  document.title = title + ' — Claurst Session';

  var msgsEl = document.getElementById('messages');

  // Tool-call visibility preference, persisted across reloads. Default: shown.
  var TOOLS_PREF_KEY = 'claurst.share.showTools';
  var showTools = true;
  try {
    var stored = localStorage.getItem(TOOLS_PREF_KEY);
    if (stored === '0') showTools = false;
  } catch (_) { /* localStorage may be unavailable in sandboxed contexts */ }
  applyShowTools(showTools);

  var hero = document.getElementById('hero');
  if (hero) {
    var heroBody = document.createElement('div');
    heroBody.className = 'hero-body';
    var h1 = document.createElement('h1');
    h1.className = 'hero-title';
    h1.textContent = title;
    heroBody.appendChild(h1);

    var exported = meta.exported_at ? new Date(meta.exported_at).toLocaleString() : '';
    var metaDiv = document.createElement('div');
    metaDiv.className = 'hero-meta';
    var bits = [];
    if (meta.model) bits.push(meta.model);
    bits.push(messages.length + ' message' + (messages.length === 1 ? '' : 's'));
    if (meta.working_dir) bits.push(meta.working_dir);
    if (exported) bits.push(exported);
    if (meta.app_version) bits.push('claurst ' + meta.app_version);
    bits.forEach(function (b) {
      var s = document.createElement('span');
      var pip = document.createElement('span');
      pip.className = 'pip';
      s.appendChild(pip);
      var label = document.createElement('span');
      label.textContent = b;
      s.appendChild(label);
      metaDiv.appendChild(s);
    });
    heroBody.appendChild(metaDiv);

    // Tool-call visibility toggle. Tool calls, tool results and thinking
    // blocks are shown by default; this lets the reader hide them for a
    // cleaner prompts/responses-only view.
    var controls = document.createElement('div');
    controls.className = 'hero-controls';
    var toolToggle = document.createElement('label');
    toolToggle.className = 'toggle';
    var cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.checked = showTools;
    cb.addEventListener('change', function () {
      showTools = cb.checked;
      applyShowTools(showTools);
      try { localStorage.setItem(TOOLS_PREF_KEY, showTools ? '1' : '0'); } catch (_) {}
    });
    toolToggle.appendChild(cb);
    var swatch = document.createElement('span');
    swatch.className = 'toggle-swatch';
    toolToggle.appendChild(swatch);
    var lbl = document.createElement('span');
    lbl.className = 'toggle-label';
    lbl.textContent = 'Show tool calls & thinking';
    toolToggle.appendChild(lbl);
    controls.appendChild(toolToggle);
    heroBody.appendChild(controls);

    hero.appendChild(heroBody);
  }

  messages.forEach(function (m) { msgsEl.appendChild(renderMessage(m)); });

  function applyShowTools(on) {
    if (on) {
      msgsEl.classList.remove('hide-tools');
    } else {
      msgsEl.classList.add('hide-tools');
    }
  }

  function renderMessage(m) {
    var div = document.createElement('div');
    var classes = ['msg', m.role || ''];
    if (isToolOnlyMessage(m)) classes.push('tool-only');
    div.className = classes.join(' ').trim();
    var role = document.createElement('div');
    role.className = 'role ' + (m.role || '');
    role.textContent = m.role || 'message';
    div.appendChild(role);
    var body = document.createElement('div');
    body.className = 'body';
    div.appendChild(body);
    renderContent(body, m.content);
    return div;
  }

  // A message is "tool-only" when every block in its content is a tool call,
  // tool result, or thinking block — i.e. there is no prose, image, or other
  // user-visible content. When the toggle hides those blocks we also hide the
  // whole row so we don't leave behind a blank user/assistant stub.
  function isToolOnlyMessage(m) {
    var content = m && m.content;
    if (typeof content === 'string') return false;
    if (!Array.isArray(content) || content.length === 0) return false;
    return content.every(isHiddenByToolToggle);
  }

  function isHiddenByToolToggle(block) {
    if (!block || typeof block !== 'object') return false;
    switch (block.type) {
      case 'tool_use':
      case 'tool_result':
      case 'thinking':
      case 'redacted_thinking':
        return true;
      case 'text':
        // Empty/whitespace-only text blocks contribute nothing visible, so
        // treat them as "no content" too — otherwise a turn that's just a
        // tool call + an empty trailing text frame would still render blank.
        return !(block.text && block.text.trim().length > 0);
      default:
        return false;
    }
  }

  function renderContent(parent, content) {
    if (typeof content === 'string') {
      parent.insertAdjacentHTML('beforeend', renderMarkdown(content));
      return;
    }
    if (!Array.isArray(content)) return;
    content.forEach(function (block) { renderBlock(parent, block); });
  }

  function renderBlock(parent, block) {
    if (!block || typeof block !== 'object') return;
    switch (block.type) {
      case 'text':
        parent.insertAdjacentHTML('beforeend', renderMarkdown(block.text || ''));
        break;
      case 'image':
        if (block.source) {
          var img = document.createElement('img');
          img.className = 'attachment';
          if (block.source.data) {
            var mt = block.source.media_type || 'image/png';
            img.src = 'data:' + mt + ';base64,' + block.source.data;
          } else if (block.source.url) {
            img.src = block.source.url;
          }
          parent.appendChild(img);
        }
        break;
      case 'tool_use':
        parent.appendChild(renderToolCall(block));
        break;
      case 'tool_result':
        parent.appendChild(renderToolResult(block));
        break;
      case 'thinking':
        parent.appendChild(renderThinking(block.thinking || ''));
        break;
      case 'redacted_thinking':
        parent.appendChild(renderThinking('[redacted thinking]'));
        break;
      case 'user_local_command_output':
        parent.appendChild(renderLocalCmd(block.command || '', block.output || ''));
        break;
      case 'user_command': {
        var d = document.createElement('div');
        d.className = 'user-cmd';
        d.textContent = '▸ ' + (block.name || '') + (block.args ? ' ' + block.args : '');
        parent.appendChild(d);
        break;
      }
      case 'user_memory_input': {
        var d2 = document.createElement('div');
        d2.className = 'user-cmd';
        d2.textContent = '# ' + (block.key || '') + ': ' + (block.value || '');
        parent.appendChild(d2);
        break;
      }
      case 'system_api_error': {
        var er = document.createElement('div');
        er.className = 'api-error';
        er.textContent = block.message || '';
        parent.appendChild(er);
        break;
      }
      case 'collapsed_read_search': {
        var cr = document.createElement('div');
        cr.className = 'user-cmd';
        var more = block.n_hidden ? ' (+ ' + block.n_hidden + ' more)' : '';
        cr.textContent = '▸ ' + (block.tool_name || 'tool') + ' ' +
          (Array.isArray(block.paths) ? block.paths.join(', ') : '') + more;
        parent.appendChild(cr);
        break;
      }
      case 'task_assignment': {
        var ta = document.createElement('details');
        ta.className = 'tool-call';
        ta.open = true;
        var s = document.createElement('summary');
        s.innerHTML = '▸ <span class="tool-name">task</span> ' + escapeHtml(block.subject || '');
        ta.appendChild(s);
        var b = document.createElement('div'); b.className = 'body';
        var pre = document.createElement('pre');
        pre.textContent = (block.id ? '[' + block.id + ']\n' : '') + (block.description || '');
        b.appendChild(pre);
        ta.appendChild(b);
        parent.appendChild(ta);
        break;
      }
      case 'document': {
        var note = document.createElement('div');
        note.style.color = 'var(--muted)';
        note.textContent = '[document: ' + (block.title || 'untitled') + ']';
        parent.appendChild(note);
        break;
      }
      default: {
        var pre2 = document.createElement('pre');
        pre2.textContent = JSON.stringify(block, null, 2);
        parent.appendChild(pre2);
      }
    }
  }

  function renderToolCall(block) {
    var d = document.createElement('details');
    d.className = 'tool-call';
    d.open = true;
    var s = document.createElement('summary');
    
    var badge = document.createElement('span');
    badge.className = 'badge tool-badge';
    badge.textContent = 'tool call';
    s.appendChild(badge);
    
    var nameSpan = document.createElement('span');
    nameSpan.className = 'tool-name';
    nameSpan.textContent = block.name || 'tool';
    s.appendChild(nameSpan);
    
    d.appendChild(s);
    var b = document.createElement('div'); b.className = 'body';
    var pre = document.createElement('pre');
    try { pre.textContent = JSON.stringify(block.input || {}, null, 2); }
    catch (_) { pre.textContent = String(block.input); }
    b.appendChild(pre);
    d.appendChild(b);
    return d;
  }

  function renderToolResult(block) {
    var isError = !!block.is_error;
    var d = document.createElement('details');
    d.className = 'tool-result' + (isError ? ' error' : '');
    d.open = true;
    var s = document.createElement('summary');
    
    var badge = document.createElement('span');
    badge.className = 'badge result-badge' + (isError ? ' error' : '');
    badge.textContent = isError ? 'error' : 'result';
    s.appendChild(badge);
    
    var labelSpan = document.createElement('span');
    labelSpan.className = 'result-label';
    labelSpan.textContent = 'tool output';
    s.appendChild(labelSpan);
    
    d.appendChild(s);
    var b = document.createElement('div'); b.className = 'body';
    var text;
    var c = block.content;
    if (typeof c === 'string') {
      text = c;
    } else if (Array.isArray(c)) {
      text = c.map(function (x) { return (x && x.text) ? x.text : JSON.stringify(x); }).join('\n');
    } else {
      text = JSON.stringify(c, null, 2);
    }
    var pre = document.createElement('pre');
    pre.textContent = text;
    b.appendChild(pre);
    d.appendChild(b);
    return d;
  }

  function renderThinking(text) {
    var d = document.createElement('details');
    d.className = 'thinking';
    var s = document.createElement('summary');
    
    var badge = document.createElement('span');
    badge.className = 'badge thinking-badge';
    badge.textContent = 'thinking';
    s.appendChild(badge);
    
    d.appendChild(s);
    var b = document.createElement('div'); b.className = 'body';
    var pre = document.createElement('pre');
    pre.textContent = text;
    b.appendChild(pre);
    d.appendChild(b);
    return d;
  }

  function renderLocalCmd(cmd, output) {
    var div = document.createElement('div');
    div.className = 'local-cmd';
    var p = document.createElement('span');
    p.className = 'prompt';
    p.textContent = '!';
    div.appendChild(p);
    div.appendChild(document.createTextNode(cmd + '\n' + output));
    return div;
  }

  function renderMarkdown(text) {
    if (window.marked) {
      try { return marked.parse(text || ''); } catch (_) { return '<p>' + escapeHtml(text) + '</p>'; }
    }
    return '<p>' + escapeHtml(text) + '</p>';
  }

  function escapeHtml(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (ch) {
      return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[ch];
    });
  }
})();
