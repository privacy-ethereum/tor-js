// Polyfill Symbol.dispose for runtimes that don't support it yet
// (e.g. Node < 20, older browsers). Must be imported before wasm-pkg
// code runs, since wasm-bindgen uses `if (Symbol.dispose)` at load time.
(Symbol as any).dispose ??= Symbol('Symbol.dispose');
