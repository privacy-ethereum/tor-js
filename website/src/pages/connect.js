import '../theme.css';
import '../tools.css';
import { mountChrome } from '../chrome.js';
import { Gateway } from '../torJsGateway.js';

mountChrome('connect');

const DEFAULT_GATEWAY = '170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww';
const $ = (id) => document.getElementById(id);
const gatewayInput = $('gateway'), targetInput = $('target');
const connectBtn = $('connect'), randomBtn = $('random');
const sendInput = $('send'), sendBtn = $('send-btn'), tlsBtn = $('tls-btn'), closeBtn = $('close-btn');
const logEl = $('log'), sentEl = $('sent'), recvEl = $('recv'), cstatEl = $('cstat');

gatewayInput.value = DEFAULT_GATEWAY;

let gw = null;
let sock = null;
let writer = null;
let sent = 0, recv = 0;

function gateway() {
  const addr = gatewayInput.value.trim();
  if (!addr) { gatewayInput.focus(); throw new Error('enter the gateway address first'); }
  if (!gw || gw.address !== addr) { gw?.close(); gw = new Gateway(addr); }
  return gw;
}

function esc(s) { return String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])); }

function log(mark, body, cls = 'log-t', isHtml = false) {
  const row = document.createElement('div');
  row.className = 'log-row';
  row.innerHTML = `<span class="log-mark ${cls}">${mark}</span><span>${isHtml ? body : esc(body)}</span>`;
  logEl.appendChild(row);
  while (logEl.children.length > 800) logEl.removeChild(logEl.firstChild);
  logEl.scrollTop = logEl.scrollHeight;
}

function setConnected(on) {
  sendInput.disabled = !on; sendBtn.disabled = !on; tlsBtn.disabled = !on; closeBtn.disabled = !on;
  connectBtn.disabled = on;
  cstatEl.textContent = on ? 'open' : 'closed';
}

connectBtn.addEventListener('click', async () => {
  const target = targetInput.value.trim();
  if (!target) { targetInput.focus(); return; }
  connectBtn.disabled = true;
  sent = recv = 0; sentEl.textContent = '0'; recvEl.textContent = '0';
  log('*', `Opening tunnel to ${target}…`, 'log-i');
  try {
    sock = await gateway().connect(target);
    writer = sock.writable.getWriter();
    setConnected(true);
    log('*', 'Tunnel open', 'log-ok');
    pump(sock.readable.getReader());
  } catch (e) {
    log('!', e?.message || String(e), 'log-err');
    connectBtn.disabled = false;
  }
});

async function pump(reader) {
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value?.length) {
        recv += value.length; recvEl.textContent = String(recv);
        log('<', hexDump(value, 320), 'log-i', true);
      }
    }
    log('*', 'Closed', 'log-t');
  } catch (e) {
    log('!', `Error: ${e?.message || e}`, 'log-err');
  }
  setConnected(false);
}

async function send(bytes) {
  try {
    await writer.write(bytes);
    sent += bytes.length; sentEl.textContent = String(sent);
    log('>', hexDump(bytes, 320), 'log-o', true);
  } catch (e) {
    log('!', `Send error: ${e?.message || e}`, 'log-err');
  }
}

sendBtn.addEventListener('click', () => {
  const bytes = parseHex(sendInput.value);
  if (!bytes) { log('!', 'Invalid hex', 'log-err'); return; }
  send(bytes); sendInput.value = '';
});
sendInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') sendBtn.click(); });
tlsBtn.addEventListener('click', () => send(buildTlsClientHello()));
closeBtn.addEventListener('click', () => { sock?.close(); });
$('clear-log').addEventListener('click', () => { logEl.innerHTML = ''; });

randomBtn.addEventListener('click', async () => {
  randomBtn.disabled = true;
  try {
    targetInput.value = await gateway().randomRelay();
    log('*', `Picked relay ${targetInput.value}`, 'log-i');
  } catch (e) {
    log('!', e?.message || String(e), 'log-err');
  } finally {
    randomBtn.disabled = false;
  }
});

// --- helpers (lifted from the old gateway website) ------------------------

function parseHex(raw) {
  const clean = raw.replace(/0x/gi, '').replace(/[\s,]+/g, '');
  if (!clean || clean.length % 2 !== 0 || /[^0-9a-fA-F]/.test(clean)) return null;
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.substr(i * 2, 2), 16);
  return out;
}

function hexDump(bytes, maxBytes) {
  const limited = maxBytes && bytes.length > maxBytes;
  const data = limited ? bytes.slice(0, maxBytes) : bytes;
  const lines = [];
  for (let i = 0; i < data.length; i += 16) {
    const chunk = data.slice(i, i + 16);
    const offset = i.toString(16).padStart(4, '0');
    const hex = Array.from(chunk, (b) => b.toString(16).padStart(2, '0')).join(' ');
    const ascii = Array.from(chunk, (b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.')).join('');
    lines.push(`<span style="color:var(--text-muted)">${offset}</span>  ${hex.padEnd(48)} <span style="color:var(--text-muted)">${esc(ascii)}</span>`);
  }
  if (limited) lines.push(`<span style="color:var(--text-muted)">… (${bytes.length - maxBytes} more bytes)</span>`);
  return lines.join('\n');
}

function buildTlsClientHello() {
  const random = crypto.getRandomValues(new Uint8Array(32));
  const sessionId = crypto.getRandomValues(new Uint8Array(32));
  const x25519Key = crypto.getRandomValues(new Uint8Array(32));
  const exts = [];
  exts.push(0x00, 0x2b, 0x00, 0x05, 0x04, 0x03, 0x04, 0x03, 0x03);
  exts.push(0x00, 0x0a, 0x00, 0x06, 0x00, 0x04, 0x00, 0x1d, 0x00, 0x17);
  exts.push(0x00, 0x0d, 0x00, 0x0a, 0x00, 0x08, 0x04, 0x03, 0x08, 0x04, 0x04, 0x01, 0x08, 0x01);
  exts.push(0x00, 0x33, 0x00, 0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20, ...x25519Key);
  const ciphers = [0x13, 0x01, 0x13, 0x02, 0x13, 0x03, 0xc0, 0x2b, 0xc0, 0x2f, 0xc0, 0x2c, 0xc0, 0x30];
  const body = [
    0x03, 0x03, ...random, 0x20, ...sessionId,
    (ciphers.length >> 8) & 0xff, ciphers.length & 0xff, ...ciphers,
    0x01, 0x00,
    (exts.length >> 8) & 0xff, exts.length & 0xff, ...exts,
  ];
  const hs = [0x01, (body.length >> 16) & 0xff, (body.length >> 8) & 0xff, body.length & 0xff, ...body];
  const record = [0x16, 0x03, 0x01, (hs.length >> 8) & 0xff, hs.length & 0xff, ...hs];
  return new Uint8Array(record);
}
