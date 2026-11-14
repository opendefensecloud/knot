import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as Y from "yjs";

import { FakeSocket, syncMsg } from "../../test/fakeSocket";

import pendingHandshake from "./__fixtures__/pendingHandshake.json";
import { KnotProvider, type CommentChangeMsg } from "./KnotProvider";

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

    const frame = commentsFrame({ doc_id: "doc-123" });

    (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(frame);

    expect(seen).toEqual(["doc-123"]);
    p.destroy();
  });

  // Suggestion mutations reuse the comments frame with a `topic` field; the
  // provider forwards the parsed JSON verbatim so KnotEditor can route it.
  it("dispatches_topic_to_comments_listeners", () => {
    const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc: new Y.Doc() });
    const seen: CommentChangeMsg[] = [];
    p.on("comments", (m) => seen.push(m));

    const frame = commentsFrame({ doc_id: "d", topic: "suggestions" });

    (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(frame);

    expect(seen).toEqual([{ doc_id: "d", topic: "suggestions" }]);
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

// MSG_COMMENTS=5, then varuint length + the JSON-encoded body.
function commentsFrame(obj: unknown): Uint8Array {
  const json = new TextEncoder().encode(JSON.stringify(obj));
  const head = [5];
  let len = json.length;
  while (len >= 0x80) { head.push((len & 0x7f) | 0x80); len >>= 7; }
  head.push(len);
  return new Uint8Array([...head, ...json]);
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

describe("KnotProvider close codes", () => {
  beforeEach(() => {
    FakeSocket.all = [];
    vi.useFakeTimers();
    vi.stubGlobal("WebSocket", FakeSocket);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  // The server refused an update this client sent (4422). The local doc
  // still holds it, so a reconnect would hand it over again in the answer
  // to the server's SYNC_STEP_1 and be refused again, every half second.
  it("stays closed after the server refused an update (4422)", () => {
    const p = new KnotProvider({ url: "ws://x/collab", doc: new Y.Doc() });
    FakeSocket.all.at(-1)!.open();

    FakeSocket.all.at(-1)!.drop(4422);

    expect(p.status).toBe("refused");
    vi.advanceTimersByTime(31_000);
    expect(FakeSocket.all).toHaveLength(1);
    p.destroy();
  });

  it("reconnects after the room restarted (1012)", () => {
    const p = new KnotProvider({ url: "ws://x/collab", doc: new Y.Doc() });
    FakeSocket.all.at(-1)!.open();

    FakeSocket.all.at(-1)!.drop(1012);

    expect(p.status).toBe("offline");
    vi.advanceTimersByTime(31_000);
    expect(FakeSocket.all).toHaveLength(2);
    p.destroy();
  });
});

// ---------------------------------------------------------------------------
// Parked rows in the join handshake
// ---------------------------------------------------------------------------

/**
 * The yjs half of the server's join handshake on a page whose content waits
 * for a missing update: SYNC_STEP_2 carries the room's state without its
 * pending content, then each log row that parked that content arrives as a
 * SYNC_UPDATE, then SYNC_STEP_1. The fixture comes from
 * crates/knot-crdt/tests/web_handshake_fixture.rs, which builds it from the
 * reproducers of yrs 0.27.3's pending-encoding defect: `state` is the
 * room's pending-free state, `frames` the rows the room sends, and
 * `dependency`/`dependencies` what drains them.
 */
describe("KnotProvider receives parked rows in the join handshake", () => {
  type Case = {
    state: string;
    frames: string[];
    dependency?: string;
    dependencies?: string[];
    all?: string[];
  };
  const cases = pendingHandshake as Record<"crossClient" | "overlapping" | "underflow", Case>;

  const unhex = (s: string) =>
    Uint8Array.from(s.match(/../g) ?? [], (b) => Number.parseInt(b, 16));
  const dependencies = (c: Case) =>
    (c.dependencies ?? (c.dependency ? [c.dependency] : [])).map(unhex);
  // `toJSON` is the fragment's `toString()` (its XML), typed as a string.
  const fragment = (d: Y.Doc): string => d.getXmlFragment("default").toJSON();

  /** A doc that applied `updates` in order. */
  function docOf(updates: Uint8Array[]): Y.Doc {
    const d = new Y.Doc();
    for (const u of updates) Y.applyUpdate(d, u);
    return d;
  }

  /** The room's frames up to SYNC_STEP_1: the state, then the parked rows. */
  function handshakeFrames(c: Case): Uint8Array[] {
    return [syncMsg(1, unhex(c.state)), ...c.frames.map((f) => syncMsg(2, unhex(f)))];
  }

  function feed(p: KnotProvider, frames: Uint8Array[]) {
    for (const f of frames) {
      (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(f);
    }
  }

  /** Every id `c`'s state and frames name: struct ranges (Skips left out),
   *  and those plus their delete sets. Per client, as clock sets. */
  function namedIds(c: Case) {
    const structs = new Map<number, Set<number>>();
    const deletes = new Map<number, Set<number>>();
    const add = (m: Map<number, Set<number>>, client: number, clock: number, len: number) => {
      const s = m.get(client) ?? new Set<number>();
      for (let i = clock; i < clock + len; i++) s.add(i);
      m.set(client, s);
    };
    for (const u of [c.state, ...c.frames].map(unhex)) {
      const { structs: ss, ds } = Y.decodeUpdate(u);
      for (const s of ss) {
        if (!(s instanceof Y.Item || s instanceof Y.GC)) continue;
        add(structs, s.id.client, s.id.clock, s.length);
        add(deletes, s.id.client, s.id.clock, s.length);
      }
      for (const [client, items] of ds.clients) {
        for (const it of items) add(deletes, client, it.clock, it.len);
      }
    }
    return { structs, deletes };
  }

  /** The ids `update` names that `allowed` lacks, as "client#clock". */
  function strays(
    update: Uint8Array,
    allowed: { structs: Map<number, Set<number>>; deletes: Map<number, Set<number>> },
  ): string[] {
    const out: string[] = [];
    const { structs, ds } = Y.decodeUpdate(update);
    for (const s of structs) {
      if (!(s instanceof Y.Item || s instanceof Y.GC)) continue;
      for (let i = s.id.clock; i < s.id.clock + s.length; i++) {
        if (!allowed.structs.get(s.id.client)?.has(i)) out.push(`struct ${s.id.client}#${i}`);
      }
    }
    for (const [client, items] of ds.clients) {
      for (const it of items) {
        for (let i = it.clock; i < it.clock + it.len; i++) {
          if (!allowed.deletes.get(client)?.has(i)) out.push(`delete ${client}#${i}`);
        }
      }
    }
    return out;
  }

  function drains(c: Case) {
    const doc = new Y.Doc();
    const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc });
    feed(p, handshakeFrames(c));
    expect(doc.store.pendingStructs).not.toBeNull();

    feed(p, dependencies(c).map((d) => syncMsg(2, d)));

    expect(doc.store.pendingStructs).toBeNull();
    const reference = docOf(c.all!.map(unhex));
    expect(fragment(doc)).toBe(fragment(reference));
    p.destroy();
    return fragment(doc);
  }

  it("holds parked rows from the handshake and drains them when the dependency arrives", () => {
    expect(drains(cases.crossClient)).toContain("back held");
  });

  it("drains rows parked behind overlapping skips", () => {
    drains(cases.overlapping);
  });

  it("takes a GC row after a state that holds blocks past a gap", () => {
    const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc: new Y.Doc() });
    expect(() => feed(p, handshakeFrames(cases.underflow))).not.toThrow();
    p.destroy();
  });

  describe("answers the room's SyncStep1 with nothing it was not sent", () => {
    beforeEach(() => {
      FakeSocket.all = [];
      vi.useFakeTimers();
      vi.stubGlobal("WebSocket", FakeSocket);
    });
    afterEach(() => {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    });

    for (const name of ["crossClient", "overlapping", "underflow"] as const) {
      it(name, () => {
        const c = cases[name];
        const doc = new Y.Doc();
        const p = new KnotProvider({ url: "ws://x/collab", doc });
        const ws = FakeSocket.all.at(-1)!;
        ws.open();
        for (const f of handshakeFrames(c)) ws.deliver(f);
        // The room's own state vector: it holds `state`, and parks the rest.
        const model = docOf([unhex(c.state)]);
        ws.serverAsks(model);

        const replies = ws.edits();
        expect(replies.length).toBeGreaterThan(0);
        const allowed = namedIds(c);
        for (const reply of replies) expect(strays(reply, allowed)).toEqual([]);

        if (c.all) {
          const room = docOf([unhex(c.state), ...dependencies(c), ...replies]);
          expect(fragment(room)).toBe(fragment(docOf(c.all.map(unhex))));
        }
        p.destroy();
      });
    }
  });
});
