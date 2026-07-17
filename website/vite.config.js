import { resolve } from 'node:path';
import { defineConfig } from 'vite';

const root = import.meta.dirname;

// Multi-page site: a marketing landing plus three tool pages. Each entry is a
// top-level .html file; GitHub Pages serves them at /, /demo.html, etc.
export default defineConfig({
  root,
  // Served under https://<user>.github.io/tor-js/ on Pages.
  base: './',
  build: {
    outDir: resolve(root, 'dist'),
    emptyOutDir: true,
    target: 'es2022',
    rollupOptions: {
      input: {
        index: resolve(root, 'index.html'),
        demo: resolve(root, 'demo.html'),
        connect: resolve(root, 'connect.html'),
        bootstrap: resolve(root, 'bootstrap.html'),
      },
    },
  },
});
