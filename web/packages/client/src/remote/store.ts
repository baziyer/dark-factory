import type { CapabilityMask } from "../manifest.js";
import type { PushSubscribeBody } from "../control.js";
import { SessionError } from "../session.js";
import { loopbackHost, relayOrigin } from "./invitation.js";

/**
 * One binding is everything this installation knows about one factory: where to
 * reach it, which daemon it must be, and — once paired — the non-exportable key
 * that is this installation's identity to that factory. There is no separate
 * installation id: the key is the identity, and it never leaves the browser.
 */
export type RemoteBinding = {
  nodeId: string;
  label: string;
  relayOrigin: string;
  host: string;
  daemonId: string;
  clientId?: string;
  publicKeySEC1?: Uint8Array;
  key?: CryptoKey;
  capabilities?: CapabilityMask;
  relayTicket?: string;
  /** This device's alert subscription, kept on every binding so a factory paired later gets it too. */
  push?: PushSubscribeBody;
};

export interface RemoteStore {
  list(): Promise<RemoteBinding[]>;
  /** The whole binding, always: the manager holds it and is its only writer. */
  put(binding: RemoteBinding): Promise<void>;
  forgetBinding(nodeId: string): Promise<void>;
  /** Erases every binding and every key this installation holds. */
  forgetDevice(): Promise<void>;
}

const DATABASE = "dark-factory-remote";
const BINDINGS = "bindings";

/** Test double and the shape every implementation must match. */
export class MemoryRemoteStore implements RemoteStore {
  #bindings = new Map<string, RemoteBinding>();

  async list(): Promise<RemoteBinding[]> { return [...this.#bindings.values()].map(copy); }

  async put(binding: RemoteBinding): Promise<void> {
    validate(binding);
    this.#bindings.set(binding.nodeId, copy(binding));
  }

  async forgetBinding(nodeId: string): Promise<void> { this.#bindings.delete(nodeId); }

  async forgetDevice(): Promise<void> { this.#bindings.clear(); }
}

/**
 * The durable store. A CryptoKey survives structured cloning into IndexedDB
 * without ever becoming exportable, which is the only reason a browser can hold
 * a long-lived factory identity at all.
 */
class IndexedDBRemoteStore implements RemoteStore {
  list(): Promise<RemoteBinding[]> {
    return this.#open<RemoteBinding[]>((database, resolve, reject) => {
      const request = database.transaction(BINDINGS, "readonly").objectStore(BINDINGS).getAll();
      request.onerror = () => reject(new SessionError("storage_unavailable"));
      request.onsuccess = () => resolve((request.result as RemoteBinding[]).filter(known));
    });
  }

  put(binding: RemoteBinding): Promise<void> {
    validate(binding);
    return this.#open<void>((database, resolve, reject) => {
      const request = database.transaction(BINDINGS, "readwrite").objectStore(BINDINGS).put(copy(binding));
      request.onerror = () => reject(new SessionError("storage_unavailable"));
      request.onsuccess = () => resolve(undefined);
    });
  }

  forgetBinding(nodeId: string): Promise<void> {
    return this.#open<void>((database, resolve, reject) => {
      const request = database.transaction(BINDINGS, "readwrite").objectStore(BINDINGS).delete(nodeId);
      request.onerror = () => reject(new SessionError("storage_unavailable"));
      request.onsuccess = () => resolve(undefined);
    });
  }

  forgetDevice(): Promise<void> {
    const indexed = globalThis.indexedDB;
    if (indexed === undefined) return Promise.reject(new SessionError("storage_unavailable"));
    return new Promise<void>((resolve, reject) => {
      const request = indexed.deleteDatabase(DATABASE);
      request.onerror = () => reject(new SessionError("storage_unavailable"));
      request.onblocked = () => reject(new SessionError("storage_unavailable"));
      request.onsuccess = () => resolve(undefined);
    });
  }

  #open<T>(work: (database: IDBDatabase, resolve: (value: T) => void, reject: (error: unknown) => void) => void): Promise<T> {
    const indexed = globalThis.indexedDB;
    if (indexed === undefined) return Promise.reject(new SessionError("storage_unavailable"));
    return new Promise<T>((resolve, reject) => {
      const open = indexed.open(DATABASE, 1);
      open.onerror = () => reject(new SessionError("storage_unavailable"));
      open.onupgradeneeded = () => { if (!open.result.objectStoreNames.contains(BINDINGS)) open.result.createObjectStore(BINDINGS, { keyPath: "nodeId" }); };
      open.onsuccess = () => {
        const database = open.result;
        try { work(database, resolve, reject); } catch { reject(new SessionError("storage_unavailable")); }
        database.close();
      };
    });
  }
}

export function createRemoteStore(): RemoteStore { return new IndexedDBRemoteStore(); }

/** A row this build cannot read is not a binding it is willing to dial. */
function known(binding: RemoteBinding): boolean {
  try { validate(binding); return true; } catch { return false; }
}

/** A row this build would not accept, told apart from a store it cannot reach. */
function validate(binding: RemoteBinding): void {
  if (binding === null || typeof binding !== "object") throw new SessionError("invalid_request");
  if (!/^[a-z2-7]{32}$/.test(binding.nodeId) || typeof binding.label !== "string" || !/^[0-9a-f]{32}$/.test(binding.daemonId)) throw new SessionError("invalid_request");
  // Only worth dialing if it still names a relay and a loopback host this build
  // would accept from an invitation.
  relayOrigin(binding.relayOrigin);
  loopbackHost(binding.host);
  if (binding.key !== undefined && binding.key.extractable) throw new SessionError("invalid_request");
  if (binding.push !== undefined && (typeof binding.push.endpoint !== "string" || typeof binding.push.public_key !== "string" || typeof binding.push.private_key !== "string")) throw new SessionError("invalid_request");
}

function copy(binding: RemoteBinding): RemoteBinding {
  const result: RemoteBinding = { ...binding };
  if (binding.publicKeySEC1 !== undefined) result.publicKeySEC1 = binding.publicKeySEC1.slice();
  return result;
}
