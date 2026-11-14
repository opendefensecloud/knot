/**
 * Toasts with a dedupe key: a second one with the same key replaces the
 * first, in place, instead of stacking.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useUi } from "./ui";

describe("ui toasts", () => {
  beforeEach(() => {
    useUi.setState({ toasts: [] });
  });

  it("stacks toasts without a key", () => {
    const { notify } = useUi.getState();
    notify("error", "one");
    notify("error", "two");
    expect(useUi.getState().toasts.map((toast) => toast.text)).toEqual(["one", "two"]);
  });

  it("replaces a toast with the same key, in place, with a new id", () => {
    const { notify } = useUi.getState();
    notify("warn", "Try again in 7 s.", { key: "limit" });
    notify("info", "Copied!");
    const firstId = useUi.getState().toasts[0]!.id;

    notify("warn", "Try again in 3 s.", { key: "limit" });

    const toasts = useUi.getState().toasts;
    expect(toasts.map((toast) => toast.text)).toEqual(["Try again in 3 s.", "Copied!"]);
    expect(toasts[0]!.key).toBe("limit");
    // A new id restarts the toast's timer.
    expect(toasts[0]!.id).not.toBe(firstId);
  });

  it("keeps a toast's own duration, and drops it when the toast is replaced without one", () => {
    const { notify } = useUi.getState();
    notify("info", "Uploads paused — continuing in 20 s…", { key: "limit", durationMs: 20_000 });
    expect(useUi.getState().toasts[0]!.durationMs).toBe(20_000);

    notify("warn", "You've reached your file storage limit.", { key: "limit" });
    expect(useUi.getState().toasts[0]!.durationMs).toBeUndefined();
  });

  it("keeps toasts with different keys apart", () => {
    const { notify } = useUi.getState();
    notify("warn", "a", { key: "limit" });
    notify("warn", "b", { key: "other" });
    expect(useUi.getState().toasts).toHaveLength(2);
  });

  it("adds a keyed toast again once the first was dismissed", () => {
    const { notify } = useUi.getState();
    notify("warn", "a", { key: "limit" });
    useUi.getState().dismiss(useUi.getState().toasts[0]!.id);
    notify("warn", "b", { key: "limit" });
    expect(useUi.getState().toasts.map((toast) => toast.text)).toEqual(["b"]);
  });
});
