/**
 * BoardProvider — y-protocol v1 WebSocket client for collaborative boards.
 *
 * Wire format mirrors the server's `crates/knot-server/src/protocol.rs`:
 *   <msg_type:u8> [<sync_subtype:u8>] <varuint length> <payload bytes>
 *
 * Only MSG_SYNC (0) and MSG_AWARENESS (1) are handled — boards do not
 * receive mention notifications. The caller (e.g. the board modal) is
 * expected to set its own awareness state `{ user, pointer }`; this
 * provider just plumbs awareness updates through the socket.
 *
 * Forked from `features/editor/KnotProvider.ts`. The shared bits will be
 * deduped once both providers settle; for now this is intentionally a
 * near-copy with the mention branches pruned.
 */

import * as Y from "yjs";
import { Awareness, encodeAwarenessUpdate, applyAwarenessUpdate } from "y-protocols/awareness";

const MSG_SYNC = 0;
const MSG_AWARENESS = 1;
const SYNC_STEP_1 = 0;
const SYNC_STEP_2 = 1;
const SYNC_UPDATE = 2;

export type BoardProviderStatus =
  | "connecting"
  | "connected"
  | "offline"
  | "unauthorised"
  | "conflict"
  | "refused";

export type BoardProviderEvents = {
  status: (s: BoardProviderStatus) => void;
  /** Fires once the first SYNC_STEP_2 has been applied. Stays "synced" thereafter,
   *  even across reconnects — the doc has authoritative remote state. */
  synced: () => void;
};

type Listeners = { [K in keyof BoardProviderEvents]: Array<BoardProviderEvents[K]> };

export class BoardProvider {
  readonly doc: Y.Doc;
  readonly awareness: Awareness;
  readonly url: string;
  status: BoardProviderStatus = "connecting";
  synced = false;
  private ws: WebSocket | null = null;
  private destroyed = false;
  private listeners: Listeners = { status: [], synced: [] };
  private reconnectAttempt = 0;
  private reconnectTimer: number | null = null;

  constructor(opts: { url: string; doc: Y.Doc; awareness?: Awareness }) {
    this.url = opts.url;
    this.doc = opts.doc;
    this.awareness = opts.awareness ?? new Awareness(opts.doc);
    this.connect();
    this.doc.on("update", this.handleDocUpdate);
    this.awareness.on("update", this.handleAwarenessUpdate);
  }

  on<K extends keyof BoardProviderEvents>(k: K, fn: BoardProviderEvents[K]) {
    this.listeners[k].push(fn);
  }
  off<K extends keyof BoardProviderEvents>(k: K, fn: BoardProviderEvents[K]) {
    this.listeners[k] = this.listeners[k].filter((f) => f !== fn) as Listeners[K];
  }

  /** Bytes the WebSocket has accepted but not yet pushed onto the wire.
   *  See `KnotProvider.pendingBytes` for the rationale. */
  pendingBytes(): number {
    return this.ws?.bufferedAmount ?? 0;
  }

  destroy() {
    this.destroyed = true;
    this.doc.off("update", this.handleDocUpdate);
    this.awareness.off("update", this.handleAwarenessUpdate);
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close();
    this.ws = null;
  }

  private setStatus(s: BoardProviderStatus) {
    this.status = s;
    this.listeners.status.forEach((fn) => fn(s));
  }

  private connect() {
    if (this.destroyed) return;
    this.setStatus("connecting");
    const ws = new WebSocket(this.url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.reconnectAttempt = 0;
      this.setStatus("connected");
      const sv = Y.encodeStateVector(this.doc);
      ws.send(encodeSync(SYNC_STEP_1, sv));
      const clients = [this.awareness.clientID];
      const ar = encodeAwarenessUpdate(this.awareness, clients);
      ws.send(encodeAwareness(ar));
    };
    ws.onmessage = (e) => this.handleFrame(new Uint8Array(e.data as ArrayBuffer));
    ws.onclose = (e) => {
      this.ws = null;
      if (this.destroyed) return;
      if (e.code === 4403) {
        this.setStatus("unauthorised");
        return;
      }
      if (e.code === 4408 || e.code === 4500) {
        this.setStatus("conflict");
        return;
      }
      // The server refused an update this doc sent. The doc still holds it,
      // so a reconnect would send it again and be refused again; a reload
      // starts over from the server's state.
      if (e.code === 4422) {
        this.setStatus("refused");
        return;
      }
      this.setStatus("offline");
      this.scheduleReconnect();
    };
    ws.onerror = () => {
      // onclose fires next; let it do the work.
    };
  }

  private scheduleReconnect() {
    if (this.destroyed) return;
    const backoff = Math.min(30_000, 500 * Math.pow(2, this.reconnectAttempt));
    const jitter = Math.random() * 300;
    this.reconnectAttempt += 1;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, backoff + jitter);
  }

  private handleFrame(buf: Uint8Array) {
    if (buf.length === 0) return;
    const type = buf[0];
    if (type === MSG_SYNC) {
      if (buf.length < 2) return;
      const subtype = buf[1];
      const [payload] = readVarBytes(buf, 2);
      if (!payload) return;
      switch (subtype) {
        case SYNC_STEP_1: {
          const update = Y.encodeStateAsUpdate(this.doc, payload);
          this.ws?.send(encodeSync(SYNC_STEP_2, update));
          return;
        }
        case SYNC_STEP_2:
        case SYNC_UPDATE:
          Y.applyUpdate(this.doc, payload, this);
          if (subtype === SYNC_STEP_2 && !this.synced) {
            this.synced = true;
            this.listeners.synced.forEach((fn) => fn());
          }
          return;
      }
    } else if (type === MSG_AWARENESS) {
      const [payload] = readVarBytes(buf, 1);
      if (payload) applyAwarenessUpdate(this.awareness, payload, this);
    }
  }

  private handleDocUpdate = (update: Uint8Array, origin: unknown) => {
    if (origin === this) return;
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(encodeSync(SYNC_UPDATE, update));
    }
  };

  private handleAwarenessUpdate = (
    { added, updated, removed }: { added: number[]; updated: number[]; removed: number[] },
    origin: unknown,
  ) => {
    if (origin === this) return;
    const clients = [...added, ...updated, ...removed];
    if (this.ws?.readyState === WebSocket.OPEN && clients.length > 0) {
      const update = encodeAwarenessUpdate(this.awareness, clients);
      this.ws.send(encodeAwareness(update));
    }
  };
}

function encodeVarUint(out: number[], v: number) {
  while (v >= 0x80) {
    out.push((v & 0x7f) | 0x80);
    v >>>= 7;
  }
  out.push(v & 0x7f);
}

function readVarUint(buf: Uint8Array, offset: number): [number, number] | null {
  let v = 0;
  let shift = 0;
  let i = offset;
  while (i < buf.length) {
    const b = buf[i]!;
    v |= (b & 0x7f) << shift;
    i += 1;
    if ((b & 0x80) === 0) return [v >>> 0, i];
    shift += 7;
    if (shift > 35) return null;
  }
  return null;
}

function readVarBytes(buf: Uint8Array, offset: number): [Uint8Array | null, number] {
  const res = readVarUint(buf, offset);
  if (!res) return [null, offset];
  const [len, after] = res;
  if (after + len > buf.length) return [null, offset];
  return [buf.subarray(after, after + len), after + len];
}

function encodeSync(subtype: number, payload: Uint8Array): Uint8Array {
  const head: number[] = [MSG_SYNC, subtype];
  encodeVarUint(head, payload.length);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}

function encodeAwareness(payload: Uint8Array): Uint8Array {
  const head: number[] = [MSG_AWARENESS];
  encodeVarUint(head, payload.length);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}
