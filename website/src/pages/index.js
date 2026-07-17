import '../theme.css';
import '../landing.css';
import { mountChrome } from '../chrome.js';
import { initHero } from '../hero.js';

mountChrome('index');

const canvas = document.getElementById('hero-canvas');
if (canvas) initHero(canvas);

// Copy-to-clipboard pills
for (const el of document.querySelectorAll('[data-copy]')) {
  el.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(el.dataset.copy);
      const label = el.querySelector('.copy-label');
      if (label) {
        const prev = label.textContent;
        label.textContent = 'copied!';
        label.classList.add('copied');
        setTimeout(() => { label.textContent = prev; label.classList.remove('copied'); }, 1400);
      }
    } catch { /* clipboard unavailable */ }
  });
}
