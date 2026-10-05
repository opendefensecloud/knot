/**
 * Boards share the editor's offline path: BoardProvider forwards edits only
 * while its socket is open, so a stroke drawn offline reaches the server only
 * when, on reconnect, the server asks with SYNC_STEP_1 and the provider
 * answers with SYNC_STEP_2 (crates/knot-server/tests/offline_sync.rs drives
 * the server half). This pins the provider's answer.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as Y from "yjs";

import { FakeSocket } from "../../test/fakeSocket";

import { BoardProvider } from "./BoardProvider";

beforeEach(() => {
  FakeSocket.all = [];
  vi.useFakeTimers();
  vi.stubGlobal("WebSocket", FakeSocket);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function handshake(server: Y.Doc): FakeSocket {
  const ws = FakeSocket.all.at(-1)!;
  ws.open();
  ws.serverState(server);
  ws.serverAsks(server);
  return ws;
}

describe("BoardProvider offline edits", () => {
  it("hands over an element added while offline when the server asks", () => {
    const server = new Y.Doc();
    server.getMap("elements").set("a", { id: "a" });
    const doc = new Y.Doc();
    const p = new BoardProvider({ url: "ws://x/boards", doc });
    handshake(server);
    expect(p.synced).toBe(true);

    FakeSocket.all.at(-1)!.drop();
    doc.getMap("elements").set("b", { id: "b" });
    vi.advanceTimersByTime(31_000);
    const ws2 = handshake(server);

    for (const u of ws2.edits()) Y.applyUpdate(server, u);
    expect([...server.getMap("elements").keys()].sort()).toEqual(["a", "b"]);
    p.destroy();
  });
});
