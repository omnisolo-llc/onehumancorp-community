// SPDX-License-Identifier: Apache-2.0
export type QueueAdapter = 'external' | 'indexeddb';
export type StoredQueueRow = { id: string; type: string; payload: unknown; timestamp: number };
type Change<T> = { writes?: StoredQueueRow[]; result: T };

export type ExternalQueueTransaction = {
  getAll: () => Promise<StoredQueueRow[]>;
  put: (row: StoredQueueRow) => Promise<void>;
};

export type ExternalQueueStorageDriver = {
  isAvailable?: () => Promise<boolean>;
  writeTransaction: <T>(work: (tx: ExternalQueueTransaction) => Promise<T>) => Promise<T>;
};

const ADAPTER_KEY = 'omnisolo_queue_adapter_v2';
let externalQueueDriver: ExternalQueueStorageDriver | null = null;

/**
 * Registers an optional edition-specific durable storage driver (for example,
 * a Cloud-managed SQLite adapter). Community Edition uses browser IndexedDB.
 */
export function registerExternalQueueStorage(
  driver: ExternalQueueStorageDriver | null,
): () => void {
  externalQueueDriver = driver;
  return () => {
    if (externalQueueDriver === driver) {
      externalQueueDriver = null;
    }
  };
}

function openIndexedDB(): Promise<IDBDatabase> {
  if (typeof window === 'undefined' || !window.indexedDB) {
    return Promise.reject(new Error('Offline storage unavailable: IndexedDB is not supported'));
  }
  return new Promise((resolve, reject) => {
    const request = window.indexedDB.open('OMNISOLO_Offline_Queue', 1);
    request.onerror = () => reject(request.error ?? new Error('Offline storage open failed'));
    request.onblocked = () => reject(new Error('Offline storage upgrade blocked'));
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains('actions')) {
        request.result.createObjectStore('actions', { keyPath: 'id' });
      }
    };
    request.onsuccess = () => resolve(request.result);
  });
}

/** A durable adapter owner is selected once under an origin-wide lock. */
export async function selectedQueueAdapter(): Promise<QueueAdapter> {
  if (typeof window === 'undefined' || !navigator.locks) {
    throw new Error('Offline storage requires cross-tab locks');
  }
  return navigator.locks.request('omnisolo_queue_adapter_v2', async () => {
    const selected = localStorage.getItem(ADAPTER_KEY);
    if (selected === 'external' || selected === 'indexeddb') return selected;
    if (selected !== null) throw new Error('Invalid offline storage owner');
    let adapter: QueueAdapter | null = null;
    if (externalQueueDriver) {
      try {
        const available = externalQueueDriver.isAvailable
          ? await externalQueueDriver.isAvailable()
          : true;
        if (available) adapter = 'external';
      } catch {
        adapter = null;
      }
    }
    if (!adapter) {
      const db = await openIndexedDB();
      db.close();
      adapter = 'indexeddb';
    }
    localStorage.setItem(ADAPTER_KEY, adapter);
    if (localStorage.getItem(ADAPTER_KEY) !== adapter) {
      throw new Error('Offline storage owner could not be persisted');
    }
    return adapter;
  });
}

/** Callback is synchronous so a read-modify-write stays inside one storage transaction. */
export async function queueTransaction<T>(
  adapter: QueueAdapter,
  work: (rows: StoredQueueRow[]) => Change<T>,
): Promise<T> {
  if (adapter === 'external') {
    const driver = externalQueueDriver;
    if (!driver) throw new Error('External offline storage adapter unavailable');
    if (driver.isAvailable) {
      const available = await driver.isAvailable();
      if (!available) throw new Error('External offline storage adapter unavailable');
    }
    return driver.writeTransaction(async (tx) => {
      const rows = await tx.getAll();
      const change = work(rows);
      for (const row of change.writes ?? []) {
        await tx.put(row);
      }
      return change.result;
    });
  }
  const db = await openIndexedDB();
  return new Promise<T>((resolve, reject) => {
    let failure: unknown;
    let result: T;
    try {
      const tx = db.transaction(['actions'], 'readwrite');
      tx.oncomplete = () => {
        db.close();
        resolve(result);
      };
      tx.onabort = () => {
        db.close();
        reject(failure ?? tx.error ?? new Error('Offline storage transaction aborted'));
      };
      tx.onerror = () => {
        failure ??= tx.error;
      };
      const store = tx.objectStore('actions');
      const request = store.getAll();
      request.onerror = () => {
        failure ??= request.error;
      };
      request.onsuccess = () => {
        try {
          const change = work(request.result as StoredQueueRow[]);
          result = change.result;
          for (const row of change.writes ?? []) store.put(row);
        } catch (error) {
          failure = error;
          tx.abort();
        }
      };
    } catch (error) {
      db.close();
      reject(error);
    }
  });
}

export async function readOtherAdapter(
  selected: QueueAdapter,
): Promise<{ rows: StoredQueueRow[]; unavailable: boolean }> {
  if (selected === 'indexeddb' && !externalQueueDriver) {
    return { rows: [], unavailable: false };
  }
  const other: QueueAdapter = selected === 'external' ? 'indexeddb' : 'external';
  try {
    return { rows: await queueTransaction(other, (rows) => ({ result: rows })), unavailable: false };
  } catch {
    return { rows: [], unavailable: true };
  }
}
