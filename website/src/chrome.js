// Shared site chrome: logo, nav, footer. Each page calls mountChrome(active).

export const ONION_SVG = `
<svg viewBox="0 0 32 32" fill="none" aria-hidden="true">
  <defs>
    <linearGradient id="onion-g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#b79dff"/>
      <stop offset="0.55" stop-color="#9d7bff"/>
      <stop offset="1" stop-color="#ff79c6"/>
    </linearGradient>
  </defs>
  <path d="M16 6.5C15.6 3.8 14 2.6 12.2 2.4M16 6.5C16.4 3.8 18 2.6 19.8 2.4" stroke="url(#onion-g)" stroke-width="2" stroke-linecap="round" fill="none"/>
  <path d="M16 6.2C21 10.2 24.4 14 24.4 19.1c0 5.3-3.7 8.9-8.4 8.9S7.6 24.4 7.6 19.1C7.6 14 11 10.2 16 6.2Z" stroke="url(#onion-g)" stroke-width="2.2" fill="rgba(157,123,255,0.08)" stroke-linejoin="round"/>
  <path d="M16 12.2c-2.8 1.6-4.4 4.2-4.4 7 0 3.2 1.9 5.9 4.4 5.9s4.4-2.7 4.4-5.9c0-2.8-1.6-5.4-4.4-7Z" stroke="url(#onion-g)" stroke-width="1.7" fill="none" opacity="0.75"/>
  <path d="M16 16.4c-1.3 1-2 2.2-2 3.4 0 1.5.9 2.7 2 2.7s2-1.2 2-2.7c0-1.2-.7-2.4-2-3.4Z" fill="url(#onion-g)"/>
</svg>`;

const GH_SVG = `<svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0016 8c0-4.42-3.58-8-8-8z"/></svg>`;

const REPO = 'https://github.com/privacy-ethereum/tor-js';

const LINKS = [
  { href: 'index.html', label: 'Home', key: 'index' },
  { href: 'demo.html', label: 'Demo', key: 'demo' },
  { href: 'connect.html', label: 'Relay tester', key: 'connect' },
  { href: 'bootstrap.html', label: 'Bootstrap', key: 'bootstrap' },
];

export function mountChrome(active) {
  const links = LINKS.map(
    (l) => `<a href="${l.href}"${l.key === active ? ' class="active"' : ''}>${l.label}</a>`,
  ).join('');

  const nav = document.createElement('header');
  nav.className = 'nav';
  nav.innerHTML = `
    <div class="nav-inner">
      <a class="brand" href="index.html">${ONION_SVG}<span class="brand-name">tor-js</span></a>
      <nav class="nav-links">
        ${links}
        <a class="gh" href="${REPO}" target="_blank" rel="noopener">${GH_SVG}<span>GitHub</span></a>
      </nav>
    </div>`;
  document.body.prepend(nav);

  const footer = document.createElement('footer');
  footer.className = 'footer';
  footer.innerHTML = `
    <div class="footer-inner">
      <span>tor-js — Tor in the browser, via <a href="https://gitlab.torproject.org/tpo/core/arti" target="_blank" rel="noopener">Arti</a> + WebAssembly.</span>
      <span class="spacer"></span>
      <a href="${REPO}" target="_blank" rel="noopener">GitHub</a>
      <a href="https://www.npmjs.com/package/tor-js" target="_blank" rel="noopener">npm</a>
      <a href="${REPO}/blob/main/PROTOCOL.md" target="_blank" rel="noopener">Protocol</a>
      <span>MIT / Apache-2.0</span>
    </div>`;
  document.body.append(footer);
}
