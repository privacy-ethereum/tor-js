// Response viewer for the demo: a compact meta bar, Body/Headers tabs, and
// content-type-aware body rendering (pretty JSON, HTML source + a locked-down
// preview, inline images, raw text) with copy/download.

const EXT = {
  'application/json': 'json', 'text/html': 'html', 'text/plain': 'txt',
  'text/css': 'css', 'application/javascript': 'js', 'text/javascript': 'js',
  'application/xml': 'xml', 'text/xml': 'xml',
  'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif',
  'image/svg+xml': 'svg', 'image/webp': 'webp',
};

const escapeHtml = (s) => String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));

function el(tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
}

function highlightJson(jsonStr) {
  return escapeHtml(jsonStr).replace(
    /("(?:\\u[a-fA-F0-9]{4}|\\[^u]|[^\\"])*"(\s*:)?|\b(?:true|false)\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g,
    (m) => {
      let cls = 'j-num';
      if (m[0] === '"') cls = /:\s*$/.test(m) ? 'j-key' : 'j-str';
      else if (m === 'true' || m === 'false') cls = 'j-bool';
      else if (m === 'null') cls = 'j-null';
      return `<span class="${cls}">${m}</span>`;
    },
  );
}

function statusClass(status) {
  if (status >= 200 && status < 300) return 's2';
  if (status >= 300 && status < 400) return 's3';
  if (status >= 400 && status < 500) return 's4';
  return 's5';
}

const fmtBytes = (n) => (n >= 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n >= 1e3 ? (n / 1e3).toFixed(1) + ' kB' : n + ' B');

/**
 * @param {HTMLElement} container
 * @param {{url:string, status:number, statusText:string, headers:Headers, bytes:Uint8Array, seconds:string}} resp
 */
export function renderResponse(container, resp) {
  const { url, status, statusText, headers, bytes, seconds } = resp;
  const ctype = (headers.get('content-type') || '').toLowerCase();
  const isImage = ctype.startsWith('image/') && !ctype.includes('svg');
  const decodable = !isImage;
  const text = decodable ? new TextDecoder('utf-8', { fatal: false }).decode(bytes) : '';

  // Detect JSON (by type, or text that parses)
  let jsonPretty = null;
  if (/json/.test(ctype) || (decodable && !ctype.includes('html') && looksJson(text))) {
    try { jsonPretty = JSON.stringify(JSON.parse(text), null, 2); } catch { /* not json */ }
  }
  const isHtml = ctype.includes('text/html');

  container.className = 'resp';
  container.innerHTML = '';

  // --- meta bar ---
  const meta = el('div', 'resp-meta');
  meta.append(
    el('span', 'resp-chip resp-status ' + statusClass(status), `${status} ${escapeHtml(statusText || '')}`),
    el('span', 'resp-chip', `⏱ ${escapeHtml(seconds)}s`),
    el('span', 'resp-chip', fmtBytes(bytes.length)),
  );
  if (ctype) meta.append(el('span', 'resp-chip', escapeHtml(ctype.split(';')[0])));
  container.append(meta);

  // --- tabs + actions ---
  const tabs = el('div', 'resp-tabs');
  const bodyTab = el('button', 'resp-tab active', 'Body');
  const hdrCount = [...headers.keys()].length;
  const headersTab = el('button', 'resp-tab', `Headers <span class="resp-tab-count">${hdrCount}</span>`);
  const actions = el('div', 'resp-actions');
  const viewControls = el('span', 'resp-view-controls');
  const copyBtn = el('button', 'btn ghost sm', 'Copy');
  const dlBtn = el('button', 'btn ghost sm', 'Download');
  actions.append(viewControls, copyBtn, dlBtn);
  tabs.append(bodyTab, headersTab, actions);
  container.append(tabs);

  const bodyPane = el('div', 'resp-pane');
  const headersPane = el('div', 'resp-pane');
  headersPane.hidden = true;
  container.append(bodyPane, headersPane);

  // --- headers table ---
  const table = el('table', 'resp-table');
  for (const [k, v] of headers.entries()) {
    const tr = document.createElement('tr');
    tr.append(el('td', 'k', escapeHtml(k)), el('td', 'v', escapeHtml(v)));
    table.append(tr);
  }
  headersPane.append(table);

  // --- body ---
  let wrap = true;
  const preClass = () => 'resp-code' + (wrap ? '' : ' nowrap');

  function segmented(options, initial, onChange) {
    const seg = el('div', 'seg');
    const btns = options.map((o) => {
      const b = el('button', o.value === initial ? 'active' : '', o.label);
      b.addEventListener('click', () => {
        for (const x of seg.children) x.classList.remove('active');
        b.classList.add('active');
        onChange(o.value);
      });
      return b;
    });
    seg.append(...btns);
    return seg;
  }

  function showCode(html) { bodyPane.innerHTML = `<pre class="${preClass()}">${html}</pre>`; }

  function renderJson(mode) {
    if (mode === 'raw') showCode(escapeHtml(text));
    else showCode(highlightJson(jsonPretty));
  }
  function renderText() { showCode(escapeHtml(text)); }

  function renderHtml(mode) {
    if (mode === 'preview') {
      bodyPane.innerHTML = '';
      // Locked down: sandbox with no scripts + a CSP that blocks every external
      // load, so nothing renders off-Tor. Layout/text only.
      const note = el('p', 'resp-note', 'Preview is sandboxed — external resources (images, scripts, styles) are blocked so nothing loads outside Tor.');
      const frame = document.createElement('iframe');
      frame.className = 'resp-preview';
      frame.setAttribute('sandbox', '');
      const csp = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">`;
      frame.srcdoc = csp + text;
      bodyPane.append(note, frame);
    } else {
      showCode(escapeHtml(text));
    }
  }

  if (isImage) {
    const blob = new Blob([bytes], { type: ctype });
    const objUrl = URL.createObjectURL(blob);
    const img = el('img', 'resp-img');
    img.src = objUrl;
    img.alt = 'response image';
    bodyPane.append(img);
    copyBtn.hidden = true;
  } else if (jsonPretty != null) {
    viewControls.append(segmented([{ label: 'Pretty', value: 'pretty' }, { label: 'Raw', value: 'raw' }], 'pretty', renderJson));
    renderJson('pretty');
  } else if (isHtml) {
    viewControls.append(segmented([{ label: 'Source', value: 'source' }, { label: 'Preview', value: 'preview' }], 'source', renderHtml));
    renderHtml('source');
  } else if (decodable) {
    renderText();
  } else {
    bodyPane.append(el('p', 'resp-note', `Binary response (${fmtBytes(bytes.length)}). Use Download to save it.`));
    copyBtn.hidden = true;
  }

  // wrap toggle (only meaningful for code views)
  if (!isImage && (jsonPretty != null || isHtml || decodable)) {
    const wrapBtn = el('button', 'btn ghost sm', 'Wrap: on');
    wrapBtn.addEventListener('click', () => {
      wrap = !wrap;
      wrapBtn.textContent = 'Wrap: ' + (wrap ? 'on' : 'off');
      const pre = bodyPane.querySelector('pre');
      if (pre) pre.className = preClass();
    });
    viewControls.append(wrapBtn);
  }

  // --- tab switching ---
  bodyTab.addEventListener('click', () => {
    bodyTab.classList.add('active'); headersTab.classList.remove('active');
    bodyPane.hidden = false; headersPane.hidden = true; viewControls.hidden = false;
  });
  headersTab.addEventListener('click', () => {
    headersTab.classList.add('active'); bodyTab.classList.remove('active');
    headersPane.hidden = false; bodyPane.hidden = true; viewControls.hidden = true;
  });

  // --- actions ---
  copyBtn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(jsonPretty ?? text);
      copyBtn.textContent = 'Copied!';
      setTimeout(() => { copyBtn.textContent = 'Copy'; }, 1300);
    } catch { /* clipboard unavailable */ }
  });
  dlBtn.addEventListener('click', () => {
    const blob = new Blob([bytes], { type: ctype || 'application/octet-stream' });
    const objUrl = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = objUrl;
    a.download = filename(url, ctype);
    a.click();
    URL.revokeObjectURL(objUrl);
  });
}

function looksJson(text) {
  const t = text.trimStart();
  return t.startsWith('{') || t.startsWith('[');
}

function filename(url, ctype) {
  let base = 'response';
  try {
    const p = new URL(url).pathname.split('/').filter(Boolean).pop();
    if (p) base = p.replace(/\.[^.]+$/, '');
  } catch { /* keep default */ }
  const ext = EXT[ctype.split(';')[0]] || 'txt';
  return `${base}.${ext}`;
}
