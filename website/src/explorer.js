// Relay explorer for the bootstrap inspector: parses the consensus into a
// searchable, virtualized relay list (joined to microdescriptors by SHA-256
// hash), plus authority certs and directory signatures. Ported from the old
// gateway website, restyled to the site's design system.

const esc = (s) => (s ? String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])) : '');
const getField = (text, key) => { const m = text.match(new RegExp('^' + key + ' (.+)$', 'm')); return m ? m[1] : null; };
const fmtBytes = (n) => (n >= 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n >= 1e3 ? (n / 1e3).toFixed(0) + ' kB' : n + ' B');

function parseConsensus(text) {
  const lines = text.split('\n');
  let i = 0;
  while (i < lines.length && !lines[i].startsWith('r ')) i++;
  const header = lines.slice(0, i).join('\n');

  const relays = [];
  while (i < lines.length) {
    if (!lines[i].startsWith('r ')) { if (lines[i] === 'directory-footer') break; i++; continue; }
    const start = i;
    const rParts = lines[i].split(' ');
    const nickname = rParts[1] || '?';
    const ip = rParts[5] || '?';
    const port = rParts[6] || '0';
    i++;
    let flags = [], bandwidth = 0, mdHash = '', rawLines = [lines[start]];
    while (i < lines.length && !lines[i].startsWith('r ') && lines[i] !== 'directory-footer') {
      rawLines.push(lines[i]);
      if (lines[i].startsWith('s ')) flags = lines[i].slice(2).split(' ');
      if (lines[i].startsWith('w ')) { const m = lines[i].match(/Bandwidth=(\d+)/); if (m) bandwidth = parseInt(m[1], 10); }
      if (lines[i].startsWith('m ')) mdHash = lines[i].split(' ')[1] || '';
      i++;
    }
    relays.push({ nickname, ip, port, flags, bandwidth, mdHash, raw: rawLines.join('\n') });
  }

  const footerText = lines.slice(i).join('\n');
  const bwWeights = getField(footerText, 'bandwidth-weights') || '';
  const signatures = [];
  for (const block of footerText.split(/(?=^directory-signature )/m)) {
    if (!block.startsWith('directory-signature ')) continue;
    const parts = block.match(/^directory-signature\s+(\S+)\s+(\S+)\s+(\S+)/);
    signatures.push({ identity: parts ? parts[2] : '?', raw: block.trim() });
  }
  return { header, relays, bwWeights, signatures };
}

function parseMicrodescSummary(md) {
  const id = getField(md, 'id ed25519') || '';
  const ntor = getField(md, 'ntor-onion-key') || '';
  const pLine = md.match(/^p6? (.+)$/m);
  return { id, ntor, policy: pLine ? pLine[1] : '' };
}

const parseAuthCert = (cert) => ({
  fingerprint: getField(cert, 'fingerprint') || 'unknown',
  expires: getField(cert, 'dir-key-expires') || '?',
});

async function hashAll(texts) {
  // crypto.subtle is present in secure contexts (https / localhost).
  const toB64 = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/=+$/, '');
  return Promise.all(texts.map(async (t) => {
    const h = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(t));
    return toB64(new Uint8Array(h));
  }));
}

function makeCollapsible(label, meta, rawText) {
  const row = document.createElement('div');
  row.className = 'item-row';
  row.innerHTML =
    `<div class="item-summary"><span class="chevron">▶</span><span class="label">${esc(label)}</span>` +
    (meta ? `<span class="meta">${esc(meta)}</span>` : '') + `</div>` +
    `<div class="item-detail"><pre class="raw-text"></pre></div>`;
  row.querySelector('.raw-text').textContent = rawText;
  row.querySelector('.item-summary').addEventListener('click', () => row.classList.toggle('open'));
  return row;
}

function createFreshnessBar(vaStr, fuStr, vuStr) {
  const va = new Date(vaStr + 'Z').getTime();
  const fu = new Date(fuStr + 'Z').getTime();
  const vu = new Date(vuStr + 'Z').getTime();
  const freshDur = fu - va, validDur = vu - fu;
  const wrap = document.createElement('div');
  wrap.className = 'freshness-bar-wrap';
  const bar = document.createElement('div');
  bar.className = 'freshness-bar';
  const fmtTime = (d) => d.toISOString().slice(11, 16);
  const mk = (cls, flex, txt) => { const s = document.createElement('div'); s.className = 'segment ' + cls; if (flex) s.style.flex = flex; s.textContent = txt; return s; };
  const segPre = mk('seg-prevalid', 0, '\nearly');
  const segFresh = mk('seg-fresh', freshDur, fmtTime(new Date(va)) + '\nfresh');
  const segValid = mk('seg-valid', validDur, fmtTime(new Date(fu)) + '\nvalid');
  const segStale = mk('seg-stale', 0, fmtTime(new Date(vu)) + '\nstale');
  bar.append(segPre, segFresh, segValid, segStale);
  const now = document.createElement('div');
  now.className = 'freshness-now';
  wrap.append(bar, now);
  let timer;
  function update() {
    if (!wrap.isConnected) { clearInterval(timer); return; }
    const t = Date.now();
    const preW = segPre.offsetWidth, freshW = segFresh.offsetWidth, validW = segValid.offsetWidth, staleW = segStale.offsetWidth;
    if (bar.offsetWidth === 0) return;
    let px, active;
    if (t < va) { px = preW / 2; active = segPre; }
    else if (t < fu) { px = preW + ((t - va) / freshDur) * freshW; active = segFresh; }
    else if (t < vu) { px = preW + freshW + ((t - fu) / validDur) * validW; active = segValid; }
    else { px = bar.offsetWidth - staleW / 2; active = segStale; }
    now.style.left = px + 'px';
    for (const s of [segPre, segFresh, segValid, segStale]) s.classList.toggle('active', s === active);
  }
  update();
  timer = setInterval(update, 1000);
  new ResizeObserver(update).observe(bar);
  return wrap;
}

function createVirtualList(container, allItems, renderRow, searchFn, renderDetail) {
  const ROW = 32, OVERSCAN = 5;
  const info = document.createElement('div');
  info.className = 'vlist-info';
  const viewport = document.createElement('div');
  viewport.className = 'vlist-viewport';
  const sentinel = document.createElement('div');
  sentinel.className = 'vlist-sentinel';
  viewport.appendChild(sentinel);
  container.append(info, viewport);

  let filtered = allItems, expandedIdx = -1, rendered = [], expandedDetail = null;
  const updateInfo = () => {
    info.textContent = filtered.length === allItems.length
      ? `${allItems.length.toLocaleString()} items`
      : `${filtered.length.toLocaleString()} / ${allItems.length.toLocaleString()} items`;
  };

  function render() {
    const scrollTop = viewport.scrollTop, viewH = viewport.clientHeight;
    sentinel.style.height = filtered.length * ROW + 'px';
    const start = Math.max(0, Math.floor(scrollTop / ROW) - OVERSCAN);
    const end = Math.min(filtered.length, Math.ceil((scrollTop + viewH) / ROW) + OVERSCAN);
    for (const el of rendered) el.remove();
    rendered = [];
    for (let i = start; i < end; i++) {
      const el = renderRow(filtered[i], i);
      el.style.top = i * ROW + 'px';
      el.style.height = ROW + 'px';
      el.addEventListener('click', () => { expandedIdx = expandedIdx === i ? -1 : i; render(); });
      viewport.appendChild(el);
      rendered.push(el);
    }
    const show = expandedIdx >= start && expandedIdx < end;
    if (show) {
      if (!expandedDetail || expandedDetail.idx !== expandedIdx) {
        expandedDetail?.el.remove();
        const detail = document.createElement('div');
        detail.className = 'vlist-expanded';
        detail.appendChild(renderDetail ? renderDetail(filtered[expandedIdx]) : (() => { const p = document.createElement('pre'); p.className = 'raw-text'; p.textContent = filtered[expandedIdx].raw || ''; return p; })());
        detail.addEventListener('click', (e) => e.stopPropagation());
        viewport.appendChild(detail);
        expandedDetail = { idx: expandedIdx, el: detail };
      }
      expandedDetail.el.style.top = (expandedIdx + 1) * ROW + 'px';
    } else if (expandedDetail) { expandedDetail.el.remove(); expandedDetail = null; }
  }

  viewport.addEventListener('scroll', render);
  new ResizeObserver(render).observe(viewport);
  updateInfo();
  render();

  return {
    setFilter(q) {
      filtered = q ? allItems.filter((it) => searchFn(it, q)) : allItems;
      expandedIdx = -1;
      expandedDetail?.el.remove(); expandedDetail = null;
      viewport.scrollTop = 0;
      updateInfo(); render();
      return filtered.length;
    },
  };
}

const rowFilter = (rows) => ({
  setFilter(q) {
    let n = 0;
    for (const row of rows) { const m = q ? row._s.includes(q) : true; row.hidden = !m; if (m) n++; }
    return n;
  },
});

function displayConsensus(consensus, block, mdHashMap, groups) {
  const va = getField(consensus, 'valid-after'), fu = getField(consensus, 'fresh-until'), vu = getField(consensus, 'valid-until');
  const parsed = parseConsensus(consensus);
  const flagCounts = {};
  for (const r of parsed.relays) for (const f of r.flags) flagCounts[f] = (flagCounts[f] || 0) + 1;

  block.innerHTML = '<h3>Consensus</h3>';
  const summary = document.createElement('div');
  summary.className = 'result-summary';
  summary.innerHTML =
    `<dl><dt>Flavor</dt><dd>microdesc</dd><dt>Valid after</dt><dd>${esc(va)}</dd>` +
    `<dt>Fresh until</dt><dd>${esc(fu)}</dd><dt>Valid until</dt><dd>${esc(vu)}</dd>` +
    `<dt>Relays</dt><dd>${parsed.relays.length.toLocaleString()}</dd></dl>`;
  if (va && fu && vu) summary.appendChild(createFreshnessBar(va, fu, vu));
  const flagBar = document.createElement('div');
  flagBar.className = 'flag-bar';
  for (const f of ['Guard', 'Exit', 'HSDir', 'Stable', 'Fast', 'Running', 'V2Dir', 'Valid']) {
    if (!flagCounts[f]) continue;
    const b = document.createElement('span');
    b.className = 'flag-badge' + (f === 'Exit' ? ' exit' : f === 'Guard' ? ' guard' : f === 'HSDir' ? ' hsdir' : '');
    b.textContent = `${f}: ${flagCounts[f].toLocaleString()}`;
    flagBar.appendChild(b);
  }
  summary.appendChild(flagBar);
  summary.appendChild(makeCollapsible('Consensus header', `${parsed.header.split('\n').length} lines`, parsed.header));
  if (parsed.bwWeights) summary.appendChild(makeCollapsible('Bandwidth weights', '', parsed.bwWeights));
  block.appendChild(summary);

  const relayItems = parsed.relays.map((r) => ({ ...r, microdesc: mdHashMap.get(r.mdHash) || null }));
  const browser = document.createElement('div');
  browser.className = 'vlist-browser';
  block.appendChild(browser);
  const group = { blockEl: block, targets: [] };

  group.targets.push(createVirtualList(browser, relayItems,
    (relay) => {
      const el = document.createElement('div');
      el.className = 'vlist-item';
      const flags = relay.flags.filter((f) => f === 'Exit' || f === 'Guard')
        .map((f) => `<span class="mini-flag ${f.toLowerCase()}">${f}</span>`).join('');
      const bw = relay.bandwidth >= 1000 ? (relay.bandwidth / 1000).toFixed(0) + 'k' : String(relay.bandwidth);
      el.innerHTML = `<span class="nickname">${esc(relay.nickname)}</span><span class="ip">${esc(relay.ip)}:${esc(relay.port)}</span><span class="mini-flags">${flags}</span><span class="bw">${bw}</span>`;
      return el;
    },
    (relay, q) => relay.nickname.toLowerCase().includes(q) || relay.ip.includes(q)
      || relay.flags.some((f) => f.toLowerCase().includes(q)) || relay.raw.toLowerCase().includes(q)
      || (relay.mdHash && relay.mdHash.toLowerCase().includes(q))
      || (relay.microdesc && relay.microdesc.raw.toLowerCase().includes(q)),
    (relay) => {
      const c = document.createElement('div');
      const p = document.createElement('pre');
      p.className = 'raw-text';
      p.textContent = relay.raw;
      c.appendChild(p);
      if (relay.microdesc) c.appendChild(makeCollapsible('Matched microdesc', relay.microdesc.id || '', relay.microdesc.raw));
      return c;
    }));

  if (parsed.signatures.length) {
    const h = document.createElement('h3');
    h.textContent = 'Directory signatures';
    h.style.marginTop = '18px';
    block.appendChild(h);
    const list = document.createElement('div');
    list.className = 'item-list';
    const rows = parsed.signatures.map((s) => { const r = makeCollapsible(s.identity, '', s.raw); r._s = s.raw.toLowerCase(); list.appendChild(r); return r; });
    block.appendChild(list);
    group.targets.push(rowFilter(rows));
  }
  groups.push(group);
}

function displayAuthCerts(authcerts, block, groups) {
  block.innerHTML = '<h3>Authority certificates</h3>';
  const summary = document.createElement('div');
  summary.className = 'result-summary';
  summary.innerHTML = `<dl><dt>Count</dt><dd>${authcerts.length}</dd></dl>`;
  block.appendChild(summary);
  const list = document.createElement('div');
  list.className = 'item-list';
  const rows = authcerts.map((cert) => { const info = parseAuthCert(cert); const r = makeCollapsible(info.fingerprint, `expires ${info.expires}`, cert); r._s = cert.toLowerCase(); list.appendChild(r); return r; });
  block.appendChild(list);
  groups.push({ blockEl: block, targets: [rowFilter(rows)] });
}

function displayMicrodescs(microdescs, mdItems, block, groups) {
  const size = microdescs.reduce((s, m) => s + m.length, 0);
  block.innerHTML = '<h3>Microdescriptors</h3>';
  const summary = document.createElement('div');
  summary.className = 'result-summary';
  summary.innerHTML = `<dl><dt>Count</dt><dd>${microdescs.length.toLocaleString()}</dd><dt>Total size</dt><dd>${fmtBytes(size)}</dd></dl>`;
  block.appendChild(summary);
  const browser = document.createElement('div');
  browser.className = 'vlist-browser';
  block.appendChild(browser);
  const vlist = createVirtualList(browser, mdItems,
    (item) => {
      const el = document.createElement('div');
      el.className = 'vlist-item';
      const policy = item.policy.length > 44 ? item.policy.slice(0, 44) + '…' : item.policy;
      el.innerHTML = `<span class="nickname">${esc(item.id || item.ntor.slice(0, 20) + '…')}</span><span class="ip">${esc(policy)}</span>`;
      return el;
    },
    (item, q) => (item.hash && item.hash.toLowerCase().includes(q)) || item.raw.toLowerCase().includes(q));
  groups.push({ blockEl: block, targets: [vlist] });
}

/**
 * Render the full explorer into the given elements.
 * @param {{consensus:string, microdescs:string[], authcerts:string[]}} docs
 * @param {{search:HTMLInputElement, consensus:HTMLElement, authcerts:HTMLElement, microdescs:HTMLElement, section:HTMLElement}} els
 */
export async function renderExplorer(docs, els) {
  const mdSummaries = docs.microdescs.map((md) => ({ ...parseMicrodescSummary(md), raw: md }));
  const hashes = await hashAll(docs.microdescs);
  const mdItems = mdSummaries.map((s, i) => ({ ...s, hash: hashes[i] }));
  const mdHashMap = new Map(mdItems.map((it) => [it.hash, it]));

  const groups = [];
  displayConsensus(docs.consensus, els.consensus, mdHashMap, groups);
  displayAuthCerts(docs.authcerts, els.authcerts, groups);
  displayMicrodescs(docs.microdescs, mdItems, els.microdescs, groups);

  const search = els.search;
  search.value = '';
  let timer;
  const apply = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      const q = search.value.toLowerCase().trim();
      els.section.classList.toggle('search-active', !!q);
      for (const g of groups) {
        let n = 0;
        for (const t of g.targets) n += t.setFilter(q);
        g.blockEl.hidden = q ? n === 0 : false;
      }
    }, 150);
  };
  search.oninput = apply;
  search.onkeydown = (e) => { if (e.key === 'Escape') { search.value = ''; apply(); search.blur(); } };
}
