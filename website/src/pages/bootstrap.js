import '../theme.css';
import '../tools.css';
import { mountChrome } from '../chrome.js';
import { Gateway, smartBootstrapDownload, parseBootstrapZip } from '../torJsGateway.js';

mountChrome('bootstrap');

const DEFAULT_GATEWAY = '170.64.236.147:12298:uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww';
const $ = (id) => document.getElementById(id);
const gatewayInput = $('gateway'), fetchBtn = $('fetch'), downloadBtn = $('download');
const progressCard = $('progress-card'), barDl = $('bar-dl'), dlTxt = $('dl-txt'), barDec = $('bar-dec'), decTxt = $('dec-txt');
const tilesEl = $('tiles'), consensusCard = $('consensus-card'), consensusEl = $('consensus');

gatewayInput.value = DEFAULT_GATEWAY;

let gw = null;
let cachedZip = null;

function gateway() {
  const addr = gatewayInput.value.trim();
  if (!addr) { gatewayInput.focus(); throw new Error('enter the gateway address first'); }
  if (!gw || gw.address !== addr) { gw?.close(); gw = new Gateway(addr); }
  return gw;
}

const fmtBytes = (n) => (n >= 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n >= 1e3 ? (n / 1e3).toFixed(0) + ' kB' : n + ' B');
const pct = (a, b) => (b ? Math.min(100, Math.round((a / b) * 100)) : 0);

function onEvent(ev) {
  switch (ev.type) {
    case 'fetch-progress':
      barDl.style.width = pct(ev.loaded, ev.total) + '%';
      dlTxt.textContent = `${fmtBytes(ev.loaded)}${ev.total ? ' / ' + fmtBytes(ev.total) : ''}`;
      break;
    case 'fetch-done':
      barDl.style.width = '100%';
      dlTxt.textContent = `${fmtBytes(ev.bytes)} downloaded`;
      break;
    case 'decompress-progress':
      barDec.style.width = pct(ev.loaded, ev.total) + '%';
      decTxt.textContent = `${fmtBytes(ev.loaded)}${ev.total ? ' / ' + fmtBytes(ev.total) : ''}`;
      break;
    case 'decompress-done':
      barDec.style.width = '100%';
      decTxt.textContent = `${fmtBytes(ev.bytes)} (${ev.method})`;
      break;
  }
}

fetchBtn.addEventListener('click', async () => {
  fetchBtn.disabled = true;
  downloadBtn.disabled = true;
  progressCard.hidden = false;
  tilesEl.hidden = true;
  consensusCard.hidden = true;
  barDl.style.width = '0'; barDec.style.width = '0';
  dlTxt.textContent = decTxt.textContent = '…';

  try {
    const gwc = gateway();
    const zip = await smartBootstrapDownload(gwc, onEvent);
    cachedZip = zip;
    const { consensus, microdescs, authcerts } = parseBootstrapZip(zip, onEvent);
    render(zip, consensus, microdescs, authcerts);
    downloadBtn.disabled = false;
  } catch (e) {
    dlTxt.textContent = 'Failed: ' + (e?.message || e);
  } finally {
    fetchBtn.disabled = false;
  }
});

downloadBtn.addEventListener('click', () => {
  if (!cachedZip) return;
  const url = URL.createObjectURL(new Blob([cachedZip], { type: 'application/zip' }));
  const a = document.createElement('a');
  a.href = url; a.download = 'bootstrap.zip';
  a.click();
  URL.revokeObjectURL(url);
});

function field(consensus, name) {
  const m = consensus.match(new RegExp(`^${name} (.+)$`, 'm'));
  return m ? m[1].trim() : '—';
}

function tile(n, l) { return `<div class="card tile"><div class="n">${n}</div><div class="l">${l}</div></div>`; }

function render(zip, consensus, microdescs, authcerts) {
  const relays = (consensus.match(/^r /gm) || []).length;
  const mdBytes = microdescs.reduce((s, m) => s + m.length, 0);
  tilesEl.innerHTML =
    tile(relays.toLocaleString(), 'relays in consensus') +
    tile(microdescs.length.toLocaleString(), 'microdescriptors') +
    tile(authcerts.length, 'authority certs') +
    tile(fmtBytes(zip.byteLength), 'archive (uncompressed zip)') +
    tile(fmtBytes(mdBytes), 'microdesc bytes');
  tilesEl.hidden = false;

  consensusEl.textContent =
    `valid-after   ${field(consensus, 'valid-after')}\n` +
    `fresh-until   ${field(consensus, 'fresh-until')}\n` +
    `valid-until   ${field(consensus, 'valid-until')}\n` +
    `\n` + consensus.split('\n').slice(0, 20).join('\n') + '\n…';
  consensusCard.hidden = false;
}
