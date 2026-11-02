import { beforeEach, describe, expect, it, vi } from "vitest";

import { suggestionsApi } from "./suggestions.api";

/** Stub fetch with an empty 200 and return the spy, so each test can assert
 *  the method, path and JSON body the client sent. */
function stubFetch() {
  return vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 200 }));
}

function sent(spy: ReturnType<typeof stubFetch>) {
  const [path, init] = spy.mock.calls[0]!;
  return {
    path,
    method: init?.method,
    body: typeof init?.body === "string" ? (JSON.parse(init.body) as unknown) : undefined,
  };
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe("suggestionsApi", () => {
  it("lists pending suggestions, or resolved ones when asked", async () => {
    const spy = stubFetch();
    await suggestionsApi.list("d 1");
    await suggestionsApi.list("d 1", true);
    expect(spy.mock.calls[0]![0]).toBe("/api/docs/d%201/suggestions");
    expect(spy.mock.calls[1]![0]).toBe("/api/docs/d%201/suggestions?include_resolved=true");
  });

  it("creates with the anchors and texts as the body", async () => {
    const spy = stubFetch();
    await suggestionsApi.create("d", {
      kind: "replace",
      position_y: "AAE=",
      position_y_end: "AAI=",
      original_text: "old",
      context_before: "before ",
      proposed_text: "new",
      reason: "typo",
    });
    expect(sent(spy)).toEqual({
      path: "/api/docs/d/suggestions",
      method: "POST",
      body: {
        kind: "replace",
        position_y: "AAE=",
        position_y_end: "AAI=",
        original_text: "old",
        context_before: "before ",
        proposed_text: "new",
        reason: "typo",
      },
    });
  });

  it("accepts with the text the owner saw", async () => {
    const spy = stubFetch();
    await suggestionsApi.accept("d", "s", "Now text");
    expect(sent(spy)).toEqual({
      path: "/api/docs/d/suggestions/s/accept",
      method: "POST",
      body: { seen_text: "Now text" },
    });
  });

  it("accepts an insertion with an empty seen_text", async () => {
    const spy = stubFetch();
    await suggestionsApi.accept("d", "s", "");
    expect(sent(spy).body).toEqual({ seen_text: "" });
  });

  it("rejects and withdraws without a body", async () => {
    const spy = stubFetch();
    await suggestionsApi.reject("d", "s");
    await suggestionsApi.withdraw("d", "s");
    expect(spy.mock.calls.map(([p, i]) => [p, i?.method, i?.body])).toEqual([
      ["/api/docs/d/suggestions/s/reject", "POST", undefined],
      ["/api/docs/d/suggestions/s/withdraw", "POST", undefined],
    ]);
  });

  it("replies with mentions, defaulting to none", async () => {
    const spy = stubFetch();
    await suggestionsApi.reply("d", "s", "hi");
    await suggestionsApi.reply("d", "s", "hi @a", ["u1"]);
    const calls = spy.mock.calls.map(([p, i]): unknown[] => [
      p,
      i?.method,
      JSON.parse(i?.body as string) as unknown,
    ]);
    expect(calls).toEqual([
      ["/api/docs/d/suggestions/s/replies", "POST", { body: "hi", mentions: [] }],
      ["/api/docs/d/suggestions/s/replies", "POST", { body: "hi @a", mentions: ["u1"] }],
    ]);
  });

  it("edits and removes a reply", async () => {
    const spy = stubFetch();
    await suggestionsApi.editReply("d", "s", "r", "fixed");
    await suggestionsApi.removeReply("d", "s", "r");
    expect(spy.mock.calls.map(([p, i]) => [p, i?.method, i?.body])).toEqual([
      ["/api/docs/d/suggestions/s/replies/r", "PATCH", JSON.stringify({ body: "fixed" })],
      ["/api/docs/d/suggestions/s/replies/r", "DELETE", undefined],
    ]);
  });
});
