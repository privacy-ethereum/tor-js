export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

interface LogConstructorParams {
  rawLog?: (level: LogLevel, ...args: unknown[]) => void;
  parentStartTime?: number;
  namePrefix?: string;
}

export class Log {
  private rawLog: (level: LogLevel, ...args: unknown[]) => void;
  private parentStartTime: number;
  private namePrefix: string;

  constructor(params: LogConstructorParams = {}) {
    this.parentStartTime = params.parentStartTime ?? Date.now();
    this.namePrefix = params.namePrefix ?? '';
    this.rawLog = params.rawLog ?? this.defaultRawLog.bind(this);
  }

  child(name: string): Log {
    const newPrefix = this.namePrefix ? `${this.namePrefix}.${name}` : name;
    return new Log({
      rawLog: this.rawLog,
      parentStartTime: this.parentStartTime,
      namePrefix: newPrefix,
    });
  }

  trace(...args: unknown[]): void {
    this.log('trace', ...args);
  }

  debug(...args: unknown[]): void {
    this.log('debug', ...args);
  }

  info(...args: unknown[]): void {
    this.log('info', ...args);
  }

  warn(...args: unknown[]): void {
    this.log('warn', ...args);
  }

  error(...args: unknown[]): void {
    this.log('error', ...args);
  }

  /** @internal Create a callback for WASM setLogCallback */
  _makeWasmCallback(): (level: string, target: string, message: string) => void {
    const levels: ReadonlySet<string> = new Set(['trace', 'debug', 'info', 'warn', 'error']);
    return (level: string, target: string, message: string) => {
      if (!levels.has(level)) {
        this.log('error', `unexpected log level from WASM: ${JSON.stringify(level)}`);
        level = 'debug';
      }
      this.child(target).log(level as LogLevel, message);
    };
  }

  private log(level: LogLevel, ...args: unknown[]): void {
    const elapsed = Date.now() - this.parentStartTime;
    const timestamp = formatTimestamp(elapsed);
    if (this.namePrefix) {
      this.rawLog(level, `[${timestamp}]`, `[${this.namePrefix}]`, ...args);
    } else {
      this.rawLog(level, `[${timestamp}]`, ...args);
    }
  }

  private defaultRawLog(level: LogLevel, ...args: unknown[]): void {
    console[level](...args);
  }
}

function formatTimestamp(elapsedMs: number): string {
  const totalSeconds = Math.floor(elapsedMs / 1000);
  const milliseconds = elapsedMs % 1000;
  const ms = String(milliseconds).padStart(3, '0');

  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (days > 0) {
    return `${days}d ${p2(hours)}:${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  if (hours > 0) {
    return `${p2(hours)}:${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  if (minutes > 0) {
    return `${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  return `${p2(seconds)}.${ms}`;
}

function p2(n: number): string {
  return String(n).padStart(2, '0');
}
