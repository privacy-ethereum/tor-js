// Onion-routing hero animation.
//
// A request travels You → Guard → Middle → Exit → Site. It leaves wrapped in
// three encryption layers (concentric rings); each relay peels one layer, so
// it arrives at the site as bare data — exactly what tor-js does in the
// browser. Canvas, ~60fps, static frame under prefers-reduced-motion.

const NODES = [
  { x: 0.07, label: 'You', kind: 'end' },
  { x: 0.30, label: 'Guard', kind: 'relay' },
  { x: 0.52, label: 'Middle', kind: 'relay' },
  { x: 0.74, label: 'Exit', kind: 'relay' },
  { x: 0.94, label: 'Site', kind: 'end' },
];
// Colors of the three encryption layers (outer → inner).
const LAYERS = ['#ff79c6', '#57d9e0', '#9d7bff'];
const SEGMENTS = NODES.length - 1; // 4 hops
const HOP_MS = 1150; // travel time per hop
const PAUSE_MS = 380; // "processing" pause at each relay
const CYCLE = SEGMENTS * (HOP_MS + PAUSE_MS) + 700;

export function initHero(canvas) {
  const ctx = canvas.getContext('2d');
  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  let W = 0, H = 0, dpr = 1;
  let particles = [];
  let bursts = [];
  let lastSeg = -1;
  let start = performance.now();

  function resize() {
    const r = canvas.getBoundingClientRect();
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    W = r.width; H = r.height;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    seedParticles();
  }

  function seedParticles() {
    const count = Math.round((W * H) / 26000);
    particles = Array.from({ length: count }, () => ({
      x: Math.random() * W,
      y: Math.random() * H,
      z: 0.3 + Math.random() * 0.7,
      vx: (Math.random() - 0.5) * 0.12,
      vy: (Math.random() - 0.5) * 0.12,
    }));
  }

  const pos = (i) => {
    const n = NODES[i];
    const x = n.x * W;
    const y = H * 0.52 - Math.sin(n.x * Math.PI) * H * 0.11;
    return { x, y };
  };

  function easeInOut(t) { return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2; }

  function drawBackground() {
    for (const p of particles) {
      p.x += p.vx; p.y += p.vy;
      if (p.x < 0) p.x += W; if (p.x > W) p.x -= W;
      if (p.y < 0) p.y += H; if (p.y > H) p.y -= H;
      ctx.beginPath();
      ctx.arc(p.x, p.y, p.z * 1.3, 0, Math.PI * 2);
      ctx.fillStyle = `rgba(157,123,255,${0.06 + p.z * 0.10})`;
      ctx.fill();
    }
  }

  function drawLinks(activeSeg, localT) {
    for (let i = 0; i < SEGMENTS; i++) {
      const a = pos(i), b = pos(i + 1);
      ctx.beginPath();
      ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y);
      ctx.strokeStyle = 'rgba(255,255,255,0.06)';
      ctx.lineWidth = 1.5;
      ctx.setLineDash([4, 6]);
      ctx.stroke();
      ctx.setLineDash([]);
      // brighten the traversed portion
      if (i < activeSeg || (i === activeSeg && localT > 0)) {
        const t = i < activeSeg ? 1 : localT;
        ctx.beginPath();
        ctx.moveTo(a.x, a.y);
        ctx.lineTo(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
        const g = ctx.createLinearGradient(a.x, a.y, b.x, b.y);
        g.addColorStop(0, 'rgba(157,123,255,0.55)');
        g.addColorStop(1, 'rgba(87,217,224,0.55)');
        ctx.strokeStyle = g;
        ctx.lineWidth = 2;
        ctx.stroke();
      }
    }
  }

  function drawNode(i, active) {
    const { x, y } = pos(i);
    const n = NODES[i];
    const r = n.kind === 'end' ? 9 : 7;
    if (active) {
      ctx.beginPath();
      ctx.arc(x, y, r + 12, 0, Math.PI * 2);
      ctx.fillStyle = 'rgba(157,123,255,0.14)';
      ctx.fill();
    }
    if (n.kind === 'end') {
      ctx.beginPath();
      roundRect(ctx, x - r, y - r, r * 2, r * 2, 4);
      ctx.fillStyle = '#110e1a';
      ctx.fill();
      ctx.strokeStyle = n.label === 'Site' ? '#57d9e0' : '#b79dff';
      ctx.lineWidth = 2; ctx.stroke();
    } else {
      ctx.beginPath();
      ctx.arc(x, y, r, 0, Math.PI * 2);
      ctx.fillStyle = '#110e1a';
      ctx.fill();
      ctx.strokeStyle = active ? '#b79dff' : '#6d4bd8';
      ctx.lineWidth = 2; ctx.stroke();
    }
    ctx.fillStyle = active ? '#ece9f6' : '#756e93';
    ctx.font = '600 12px Inter, system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.fillText(n.label, x, y + r + 18);
  }

  function drawPacket(x, y, layersLeft, phase) {
    // core
    ctx.beginPath();
    ctx.arc(x, y, 4.5, 0, Math.PI * 2);
    ctx.fillStyle = '#fff';
    ctx.shadowColor = '#b79dff';
    ctx.shadowBlur = 14;
    ctx.fill();
    ctx.shadowBlur = 0;
    // remaining encryption layers as concentric rings
    for (let l = 0; l < layersLeft; l++) {
      const rr = 8 + l * 5.5 + Math.sin(phase * 2 + l) * 0.8;
      ctx.beginPath();
      ctx.arc(x, y, rr, 0, Math.PI * 2);
      ctx.strokeStyle = LAYERS[l];
      ctx.globalAlpha = 0.85 - l * 0.12;
      ctx.lineWidth = 2;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }
  }

  function drawBursts(now) {
    bursts = bursts.filter((b) => now - b.t < 620);
    for (const b of bursts) {
      const k = (now - b.t) / 620;
      ctx.beginPath();
      ctx.arc(b.x, b.y, 8 + k * 34, 0, Math.PI * 2);
      ctx.strokeStyle = b.color;
      ctx.globalAlpha = (1 - k) * 0.7;
      ctx.lineWidth = 2;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }
  }

  function frame(now) {
    ctx.clearRect(0, 0, W, H);
    drawBackground();

    const elapsed = (now - start) % CYCLE;
    // resolve which segment we're on and local progress
    let acc = 0, seg = SEGMENTS - 1, localT = 1, traveling = false;
    for (let i = 0; i < SEGMENTS; i++) {
      if (elapsed < acc + HOP_MS) { seg = i; localT = easeInOut((elapsed - acc) / HOP_MS); traveling = true; break; }
      acc += HOP_MS;
      if (elapsed < acc + PAUSE_MS) { seg = i; localT = 1; traveling = false; break; }
      acc += PAUSE_MS;
    }

    // peel burst when we land on a relay (segment boundary reached)
    if (seg !== lastSeg && seg > 0) {
      const p = pos(seg);
      bursts.push({ x: p.x, y: p.y, t: now, color: LAYERS[seg - 1] });
      lastSeg = seg;
    }
    if (elapsed < 60) lastSeg = -1; // reset each cycle

    drawLinks(seg, localT);
    drawBursts(now);

    const a = pos(seg), b = pos(seg + 1);
    const x = a.x + (b.x - a.x) * localT;
    const y = a.y + (b.y - a.y) * localT;
    // layers remaining while traversing segment `seg` = 3 - seg (0 after exit)
    const layersLeft = Math.max(0, LAYERS.length - seg);

    for (let i = 0; i < NODES.length; i++) drawNode(i, i === seg || i === seg + 1);
    drawPacket(x, y, layersLeft, now / 320);

    raf = requestAnimationFrame(frame);
  }

  function staticFrame() {
    ctx.clearRect(0, 0, W, H);
    drawBackground();
    drawLinks(1, 0.5);
    for (let i = 0; i < NODES.length; i++) drawNode(i, i === 1 || i === 2);
    const a = pos(1), b = pos(2);
    drawPacket((a.x + b.x) / 2, (a.y + b.y) / 2, 2, 0);
  }

  let raf = 0;
  const ro = new ResizeObserver(() => { resize(); if (reduce) staticFrame(); });
  ro.observe(canvas);
  resize();
  if (reduce) { staticFrame(); }
  else { raf = requestAnimationFrame(frame); }
}

function roundRect(ctx, x, y, w, h, r) {
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
}
