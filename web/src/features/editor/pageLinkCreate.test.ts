/**
 * Creating a subpage from the `[[` picker: the page goes under the current
 * one, a refused or failed request is said in a toast and resolves null,
 * so the typed text stays.
 */

import { describe, expect, it, vi } from "vitest";

import type { ApiResult } from "../../lib/api";
import { subpageCreator } from "./pageLinkCreate";

function deps(create: () => Promise<ApiResult<unknown>>) {
  return {
    create: vi.fn(create),
    refreshDocs: vi.fn(),
    notify: vi.fn(),
  };
}

describe("subpageCreator", () => {
  it("creates the page under the current one and returns it", async () => {
    const d = deps(() => Promise.resolve({ ok: { id: "n", title: "Neu" } }));
    await expect(subpageCreator("cur", d)("Neu")).resolves.toEqual({ id: "n", title: "Neu" });
    expect(d.create).toHaveBeenCalledWith({ title: "Neu", parent_id: "cur" });
    expect(d.refreshDocs).toHaveBeenCalledTimes(1);
  });

  it("says so and resolves null when the server refuses", async () => {
    const d = deps(() => Promise.resolve({ error: { status: 403, code: "forbidden", message: "", details: {} } }));
    await expect(subpageCreator("cur", d)("Neu")).resolves.toBeNull();
    expect(d.notify).toHaveBeenCalledWith("error", "Couldn't create the page.");
  });

  it("says so and resolves null when the network fails", async () => {
    const d = deps(() => Promise.reject(new TypeError("Failed to fetch")));
    await expect(subpageCreator("cur", d)("Neu")).resolves.toBeNull();
    expect(d.notify).toHaveBeenCalledWith("error", "Couldn't create the page.");
  });
});
