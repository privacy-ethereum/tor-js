import { TorClient } from './TorClient.js';
import type { TorClientOptions, FetchInit } from './types.js';

let client: TorClient | undefined;

let config: TorClientOptions = {};

export const tor = {
  /**
   * Make an HTTP fetch request through Tor.
   * Automatically opens the TorClient on first use.
   */
  async fetch(url: string, init?: FetchInit): Promise<Response> {
    if (!client) {
      this.open();
    }
    return client!.fetch(url, init);
  },

  /**
   * Configure the singleton TorClient.
   * If already open, closes and reopens with the new config.
   */
  configure(options: TorClientOptions): void {
    config = options;
    if (client) {
      client.close();
      client = undefined;
      this.open();
    }
  },

  /**
   * Open the singleton TorClient.
   * Optional — fetch() calls this automatically.
   * Call this early if you know you'll need Tor, to start bootstrapping sooner.
   */
  open(): void {
    if (client) return;
    client = new TorClient(config);
  },

  /**
   * Close the singleton TorClient and release resources.
   */
  close(): void {
    if (client) {
      client.close();
      client = undefined;
    }
  },
};
