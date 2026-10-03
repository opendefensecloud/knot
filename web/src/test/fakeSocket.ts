/**
 * A scriptable stand-in for the browser WebSocket, for the y-protocol
 * providers (KnotProvider, BoardProvider). Records what the client sends and
 * lets the test play the server's side of the conversation.
 *
 * Install with `vi.stubGlobal("WebSocket", FakeSocket)`; every socket the
 * provider opens lands in `FakeSocket.all`.
 */

import * as Y from "yjs";

export class FakeSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static all: FakeSocket[] = [];
  readyState = FakeSocket.CONNECTING;
  binaryType = "blob";
  sent: Uint8Array[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: ArrayBuffer }) => void) | null = null;
  onclose: ((e: { code: number }) => void) | null = null;
  onerror: (() => void) | null = null;
  constructor(readonly url: string) {
    FakeSocket.all.push(this);
  }
  send(b: Uint8Array) {
    this.sent.push(b);
  }
  close() {
    this.readyState = FakeSocket.CLOSED;
  }
  // --- server side ---
  open() {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }
  /** The server's opening move: its whole state as SYNC_STEP_2. */
  serverState(server: Y.Doc) {
    this.deliver(syncMsg(1, Y.encodeStateAsUpdate(server)));
  }
  /** The server asking for what it lacks: SYNC_STEP_1 with its state
   *  vector. The provider answers with a SYNC_STEP_2. */
  serverAsks(server: Y.Doc) {
    this.deliver(syncMsg(0, Y.encodeStateVector(server)));
  }
  /** A frame from the server, as the arraybuffer the provider asks for. */
  deliver(frame: Uint8Array) {
    const data = new ArrayBuffer(frame.byteLength);
    new Uint8Array(data).set(frame);
    this.onmessage?.({ data });
  }
  drop() {
    this.readyState = FakeSocket.CLOSED;
    this.onclose?.({ code: 1006 });
  }
  /** Payloads of the document-carrying frames the client sent (SYNC_STEP_2
   *  and SYNC_UPDATE — the server treats both as edits). */
  edits(): Uint8Array[] {
    return this.sent.flatMap((f) => {
      if (f[0] !== 0 || (f[1] !== 1 && f[1] !== 2)) return [];
      return [readPayload(f, 2)];
    });
  }
}

/** MSG_SYNC, `subtype`, varuint length, payload — mirrors the providers'
 *  own encoder. */
export function syncMsg(subtype: number, payload: Uint8Array): Uint8Array {
  const head: number[] = [0, subtype];
  let n = payload.length;
  while (n >= 0x80) {
    head.push((n & 0x7f) | 0x80);
    n >>>= 7;
  }
  head.push(n);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}

export function readPayload(frame: Uint8Array, offset: number): Uint8Array {
  let len = 0;
  let shift = 0;
  let i = offset;
  for (;;) {
    const b = frame[i++]!;
    len |= (b & 0x7f) << shift;
    if ((b & 0x80) === 0) break;
    shift += 7;
  }
  return frame.subarray(i, i + len);
}
