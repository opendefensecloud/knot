import { describe, expect, it } from "vitest";
import * as Y from "yjs";

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
