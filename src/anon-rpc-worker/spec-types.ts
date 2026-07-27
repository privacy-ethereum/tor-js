// Worker-facing type surface from anon-rpc SPEC.md (§7–§13): the `anonRpcWorker`
// capability API that is a worker's entire platform.
//
// This file is deliberately a COPY of anon-rpc's worker-facing spec types, not
// an import. A worker is a standalone artifact — identified by the hash of its
// bundled bytes (§4) and buildable with no dependency on any particular harness.
// Keep in sync with anon-rpc/impl/passthrough-worker/src/spec-types.ts.

/* §7 — Capability API exposed to worker code as `anonRpcWorker` */

export type AnonRpcWorkerApi = {
  signalReady(): void;
  signalFailed(reason?: { code?: string; message?: string }): void;
  acceptCall(opts?: { signal?: AbortSignal }): Promise<IncomingCall>;
  // The host's WorkerInit.config (§7.1): a structured clone, fixed for the
  // worker's lifetime; undefined when the host supplied none.
  config: unknown;
  kps: KpsApi;
  storage: StorageApi;
  log: LogApi;
};

/* §8 — Inbound calls */

export type IncomingCall = FetchCall; // discriminated by `kind`; more kinds later

export type FetchCall = {
  kind: "fetch";
  url: string;
  requestInit?: AnonRequestInit;
  respond(response: AnonFetchResponse | Promise<AnonFetchResponse>): void;
};

/* §9 — Fetch payloads */

export type HeaderList = [name: string, value: string][];

export type ByteBody = Uint8Array | ReadableStream<Uint8Array>;

export type AnonRequestInit = {
  method?: string;
  headers?: HeaderList;
  body?: ByteBody;
  redirect?: "follow" | "manual" | "error";
  signal?: AbortSignal;
};

export type AnonFetchResponse = {
  status: number;
  headers: HeaderList;
  body: ByteBody;
  url?: string;
};

/* §10 — KPS transport */

export type KpsAddr = string; // "<ip>:<port>:<certhash>"

export type KpsDialOptions = { signal?: AbortSignal };
export type KpsOpenStreamOptions = { signal?: AbortSignal };

export type KpsApi = {
  dial(addr: KpsAddr, opts?: KpsDialOptions): Promise<KpsConn>;
  openStream(addr: KpsAddr, opts?: KpsDialOptions): Promise<KpsStream>;
};

export type KpsConn = {
  // The peer's UDP endpoint as observed at connection establishment (on the
  // dial side, the dialed endpoint); MAY change over the connection's life.
  // Informational only — trust derives solely from the pinned certhash.
  remoteAddress: { ip: string; port: number };
  openStream(opts?: KpsOpenStreamOptions): Promise<KpsStream>;
  acceptStream(opts?: { signal?: AbortSignal }): Promise<KpsStream>;
  sendDatagram(data: Uint8Array, opts?: { signal?: AbortSignal }): Promise<void>;
  receiveDatagram(opts?: { signal?: AbortSignal }): Promise<Uint8Array>;
  close(reason?: KpsReason): Promise<void>;
  closed: Promise<KpsConnCloseInfo>;
};

export type KpsStream = {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
  closeWrite(): Promise<void>;
  cancelRead(reason?: KpsReason): Promise<void>;
  resetWrite(reason?: KpsReason): Promise<void>;
  close(reason?: KpsReason): Promise<void>;
  closed: Promise<KpsStreamCloseInfo>;
};

export type KpsErrorCode =
  | "cancelled"
  | "closed"
  | "reset"
  | "timeout"
  | "network-error"
  | "protocol-error"
  | "unsupported"
  | "too-large"
  | "queue-full"
  | "permission-denied"
  | "internal-error";

export type KpsReason = { code?: KpsErrorCode; message?: string };
export type KpsConnCloseInfo = { ok: boolean; reason?: KpsReason };
export type KpsStreamCloseInfo = { ok: boolean; reason?: KpsReason };

/* §11 — Storage */

export type StorageKey = string;

export type StorageApi = {
  get(key: StorageKey, opts?: { signal?: AbortSignal }): Promise<Uint8Array | undefined>;
  set(key: StorageKey, value: Uint8Array, opts?: { signal?: AbortSignal }): Promise<void>;
  delete(key: StorageKey, opts?: { signal?: AbortSignal }): Promise<void>;
  has(key: StorageKey, opts?: { signal?: AbortSignal }): Promise<boolean>;
  list(opts?: { prefix?: string; signal?: AbortSignal }): AsyncIterable<StorageKey>;
  clear(opts?: { prefix?: string; signal?: AbortSignal }): Promise<void>;
};

/* §13 — Logging */

export type LogArg =
  | string
  | number
  | boolean
  | null
  | undefined
  | Uint8Array
  | LogArg[]
  | { [key: string]: LogArg };

export type LogApi = {
  debug(...args: LogArg[]): void;
  info(...args: LogArg[]): void;
  warn(...args: LogArg[]): void;
  error(...args: LogArg[]): void;
};
