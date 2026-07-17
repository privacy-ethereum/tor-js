import '../theme.css';
import '../tools.css';
import { mountChrome } from '../chrome.js';
import { renderResponse } from '../responseView.js';

mountChrome('demo');
document.body.classList.add('has-drawer');

const DEFAULT_GATEWAY = '170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww';

const $ = (id) => document.getElementById(id);
const dot = $('dot'), statusEl = $('status');
const gatewayInput = $('gateway'), connectBtn = $('connect'), disconnectBtn = $('disconnect');
const stepRequest = $('step-request');
const stepResponse = $('step-response');
const presetSel = $('preset'), customField = $('custom-field'), customUrl = $('custom-url');
const fetchBtn = $('fetch'), responseEl = $('response');
const logEl = $('log'), logDrawer = $('log-drawer'), logToggle = $('log-toggle');
const logLatest = $('log-latest'), logCount = $('log-count');

gatewayInput.value = DEFAULT_GATEWAY;

let client = null;
let TorLib = null; // lazily imported so the 2.3 MB WASM only loads on connect
let count = 0;

function setStatus(state, text) {
  dot.className = 'dot' + (state ? ' ' + state : '');
  statusEl.textContent = text;
}

function log(level, msg) {
  const row = document.createElement('div');
  row.className = 'log-row';
  const cls = { info: 'log-i', error: 'log-err', warn: 'log-err', debug: 'log-t' }[level] || 'log-t';
  const mark = level[0].toUpperCase();
  row.innerHTML = `<span class="log-mark ${cls}">${mark}</span><span>${escapeHtml(msg)}</span>`;
  logEl.appendChild(row);
  while (logEl.children.length > 500) logEl.removeChild(logEl.firstChild);
  logEl.scrollTop = logEl.scrollHeight;
  // Collapsed drawer shows just the latest line (CSS truncates it).
  logLatest.textContent = `${mark} · ${msg}`;
  logCount.textContent = String(++count);
}

function escapeHtml(s) {
  return String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]));
}

logToggle.addEventListener('click', () => {
  const open = logDrawer.classList.toggle('open');
  logToggle.setAttribute('aria-expanded', String(open));
  if (open) logEl.scrollTop = logEl.scrollHeight;
});
$('clear-log').addEventListener('click', () => {
  logEl.innerHTML = '';
  count = 0;
  logCount.textContent = '0';
  logLatest.textContent = 'Cleared';
});

presetSel.addEventListener('change', () => {
  const custom = presetSel.value === 'custom';
  customField.hidden = !custom;
  if (custom) customUrl.focus();
});

const targetUrl = () => (presetSel.value === 'custom' ? customUrl.value.trim() : presetSel.value);

connectBtn.addEventListener('click', async () => {
  const gateway = gatewayInput.value.trim();
  if (!gateway) { log('error', 'Gateway address is required'); return; }

  connectBtn.disabled = true;
  gatewayInput.disabled = true;
  setStatus('loading', 'Connecting…');
  log('info', 'Loading tor-js…');

  try {
    if (!TorLib) TorLib = await import('tor-js/wasm-base64');
    const { TorClient, Log } = TorLib;
    const t0 = performance.now();
    client = new TorClient({
      gateway,
      log: new Log({ rawLog: (level, ...args) => log(level, args.join(' ')) }),
      logLevel: 'info',
    });
    await client.ready();
    const secs = ((performance.now() - t0) / 1000).toFixed(1);
    setStatus('ok', `Connected in ${secs}s`);
    log('info', `Ready in ${secs}s`);
    disconnectBtn.disabled = false;
    stepRequest.classList.remove('locked');
    stepResponse.classList.remove('locked');
  } catch (e) {
    setStatus('err', 'Connection failed');
    log('error', e?.message || String(e));
    connectBtn.disabled = false;
    gatewayInput.disabled = false;
    client = null;
  }
});

disconnectBtn.addEventListener('click', () => {
  client?.close();
  client = null;
  setStatus('', 'Disconnected');
  connectBtn.disabled = false;
  gatewayInput.disabled = false;
  disconnectBtn.disabled = true;
  stepRequest.classList.add('locked');
  stepResponse.classList.add('locked');
});

fetchBtn.addEventListener('click', async () => {
  const url = targetUrl();
  if (!url) { log('error', 'Enter a URL'); return; }
  fetchBtn.disabled = true;
  responseEl.className = 'response empty';
  responseEl.textContent = 'Requesting…';
  log('info', `Fetching ${url}`);
  const t0 = performance.now();
  try {
    const res = await client.fetch(url);
    const bytes = new Uint8Array(await res.arrayBuffer());
    const secs = ((performance.now() - t0) / 1000).toFixed(1);
    log('info', `${res.status} in ${secs}s (${bytes.length} bytes)`);
    renderResponse(responseEl, { url, status: res.status, statusText: res.statusText, headers: res.headers, bytes, seconds: secs });
  } catch (e) {
    log('error', e?.message || String(e));
    responseEl.className = 'response';
    responseEl.innerHTML = `<span class="rstatus bad">error</span><div class="rbody">${escapeHtml(e?.message || String(e))}</div>`;
  } finally {
    fetchBtn.disabled = false;
  }
});
