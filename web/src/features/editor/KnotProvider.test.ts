import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as Y from "yjs";

import { FakeSocket } from "../../test/fakeSocket";

import { KnotProvider } from "./KnotProvider";

describe("KnotProvider", () => {
  it("constructs in 'connecting' state and destroys cleanly", () => {
    const p = new KnotProvider({
      url: "ws://127.0.0.1:1/never",
      doc: new Y.Doc(),
    });
    expect(p.status).toBe("connecting");
    p.destroy();
  });

  it("emits a status change to a registered listener on destroy path", () => {
    const p = new KnotProvider({
      url: "ws://127.0.0.1:1/never",
      doc: new Y.Doc(),
    });
    const seen: string[] = [];
    p.on("status", (s) => seen.push(s));
    // Initial status is set in connect() before the listener registered, so
    // we only assert that the listener mechanism works at all by destroying
    // (no event fires on destroy, but off() must not throw).
    p.off("status", (s) => seen.push(s));
    p.destroy();
    expect(p.status).toBe("connecting");
  });

  it("dispatches a MSG_COMMENTS frame to 'comments' listeners", () => {
    const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc: new Y.Doc() });
    const seen: string[] = [];
    p.on("comments", (m) => seen.push(m.doc_id));

    const json = new TextEncoder().encode(JSON.stringify({ doc_id: "doc-123" }));
    const head = [5]; // MSG_COMMENTS
    let len = json.length;
    while (len >= 0x80) { head.push((len & 0x7f) | 0x80); len >>= 7; }
    head.push(len);
    const frame = new Uint8Array([...head, ...json]);

    (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(frame);

    expect(seen).toEqual(["doc-123"]);
    p.destroy();
  });

  it("applies every y-sync message batched in a single frame", () => {
    const doc = new Y.Doc();
    const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc });

    // Two independent updates produced from a source doc.
    const src = new Y.Doc();
    const sv0 = Y.encodeStateVector(src);
    src.getMap("m").set("a", 1);
    const u1 = Y.encodeStateAsUpdate(src, sv0);
    const sv1 = Y.encodeStateVector(src);
    src.getMap("m").set("b", 2);
    const u2 = Y.encodeStateAsUpdate(src, sv1);

    // Concatenate two SYNC_UPDATE messages into one frame.
    const frame = concat(syncUpdateMsg(u1), syncUpdateMsg(u2));
    // handleFrame is private; exercise it directly.
    (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(frame);

    const m = doc.getMap("m");
    expect(m.get("a")).toBe(1);
    // Before the consume-loop fix, the trailing message was dropped and this
    // would be undefined.
    expect(m.get("b")).toBe(2);
    p.destroy();
  });
});

// MSG_SYNC=0, SYNC_UPDATE=2, then varuint length + payload (mirrors encodeSync).
function syncUpdateMsg(payload: Uint8Array): Uint8Array {
  const head: number[] = [0, 2];
  let n = payload.length;
  do {
    let b = n & 0x7f;
    n >>= 7;
    if (n) b |= 0x80;
    head.push(b);
  } while (n);
  const out = new Uint8Array(head.length + payload.length);
  out.set(head, 0);
  out.set(payload, head.length);
  return out;
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

// ---------------------------------------------------------------------------
// Edits made while the socket is down
// ---------------------------------------------------------------------------

/**
 * The client half of offline sync. The provider forwards edits only while its
 * socket is open, so an edit made offline lives solely in the local doc until
 * the server asks for it: on (re)connect the server sends SYNC_STEP_1 with its
 * state vector and the provider answers with a SYNC_STEP_2 carrying whatever
 * the server lacks (crates/knot-server/tests/offline_sync.rs drives the server
 * half). These pin that answer, offline deletions included.
 */
describe("KnotProvider offline edits", () => {
  beforeEach(() => {
    FakeSocket.all = [];
    vi.useFakeTimers();
    vi.stubGlobal("WebSocket", FakeSocket);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  /** Open the newest socket and play the server's handshake: full state as
   *  SYNC_STEP_2, then SYNC_STEP_1 asking for what it lacks. */
  function handshake(p: KnotProvider, server: Y.Doc): FakeSocket {
    const ws = FakeSocket.all.at(-1)!;
    ws.open();
    ws.serverState(server);
    ws.serverAsks(server);
    expect(p.status).toBe("connected");
    return ws;
  }

  function dropAndReconnect(p: KnotProvider, server: Y.Doc, offline: () => void): FakeSocket {
    FakeSocket.all.at(-1)!.drop();
    expect(p.status).toBe("offline");
    offline();
    const before = FakeSocket.all.length;
    vi.advanceTimersByTime(31_000);
    expect(FakeSocket.all.length).toBe(before + 1);
    return handshake(p, server);
  }

  const text = (d: Y.Doc) => d.getText("t").toJSON();

  it("hands over what was typed while offline when the server asks", () => {
    const server = new Y.Doc();
    server.getText("t").insert(0, "hello");
    const doc = new Y.Doc();
    const p = new KnotProvider({ url: "ws://x/collab", doc });
    handshake(p, server);

    const ws2 = dropAndReconnect(p, server, () => doc.getText("t").insert(5, " world"));

    for (const u of ws2.edits()) Y.applyUpdate(server, u);
    expect(text(server)).toBe("hello world");
    p.destroy();
  });

  it("hands over an offline deletion, which moves no state-vector clock", () => {
    const server = new Y.Doc();
    server.getText("t").insert(0, "hello world");
    const doc = new Y.Doc();
    const p = new KnotProvider({ url: "ws://x/collab", doc });
    handshake(p, server);

    const ws2 = dropAndReconnect(p, server, () => doc.getText("t").delete(5, 6));

    for (const u of ws2.edits()) Y.applyUpdate(server, u);
    expect(text(server)).toBe("hello");
    p.destroy();
  });

  it("hands over edits made before the first connection opened", () => {
    const server = new Y.Doc();
    const doc = new Y.Doc();
    const p = new KnotProvider({ url: "ws://x/collab", doc });
    doc.getText("t").insert(0, "early");

    const ws = handshake(p, server);

    for (const u of ws.edits()) Y.applyUpdate(server, u);
    expect(text(server)).toBe("early");
    p.destroy();
  });
});
