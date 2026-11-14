import { describe, expect, it, vi } from "vitest";

import { LIMIT_TOAST_KEY, limitErrorText, notifyFailure, retryAfterSecs } from "./limitErrors";

function rateLimited(details: Record<string, unknown> = { retry_after_secs: 7 }) {
  return { status: 429, code: "rate.limited", details };
}

function quotaExceeded(quota: string, scope: string) {
  return { status: 403, code: "quota.exceeded", details: { quota, scope, limit: 100 } };
}

describe("limitErrorText", () => {
  it("names the seconds a rate limit asks to wait", () => {
    expect(limitErrorText(rateLimited())).toBe("You're doing that too fast. Try again in 7 s.");
  });

  it("rounds a fractional wait up to whole seconds", () => {
    expect(limitErrorText(rateLimited({ retry_after_secs: 2.2 }))).toBe(
      "You're doing that too fast. Try again in 3 s.",
    );
  });

  it.each<[string, Record<string, unknown> | undefined]>([
    ["no details", undefined],
    ["no seconds", {}],
    ["zero seconds", { retry_after_secs: 0 }],
    ["negative seconds", { retry_after_secs: -4 }],
    ["seconds as text", { retry_after_secs: "7" }],
    ["not a number", { retry_after_secs: Number.NaN }],
  ])("says 'in a moment' with %s", (_what, details) => {
    expect(limitErrorText({ status: 429, code: "rate.limited", details })).toBe(
      "You're doing that too fast. Try again in a moment.",
    );
  });

  it.each([
    ["pages", "user", "You've reached your page limit."],
    ["pages", "workspace", "This workspace has reached its page limit."],
    ["blob_bytes", "user", "You've reached your file storage limit."],
    ["blob_bytes", "workspace", "This workspace has reached its file storage limit."],
  ])("names the %s quota at %s scope", (quota, scope, text) => {
    expect(limitErrorText(quotaExceeded(quota, scope))).toBe(text);
  });

  it("still says a quota was hit when the details are unknown", () => {
    expect(limitErrorText({ status: 403, code: "quota.exceeded", details: {} })).toBe(
      "You've reached a storage limit.",
    );
  });

  it.each([
    ["another 429", { status: 429, code: "auth.throttled", details: {} }],
    ["another 403", { status: 403, code: "acl.no_grant", details: {} }],
    ["a 503 busy", { status: 503, code: "export.busy", details: {} }],
    ["a network failure", { status: 0, code: "network", details: {} }],
  ])("returns null for %s", (_what, error) => {
    expect(limitErrorText(error)).toBeNull();
  });
});

describe("retryAfterSecs", () => {
  it("reads whole seconds from a rate-limit error", () => {
    expect(retryAfterSecs(rateLimited({ retry_after_secs: 12 }))).toBe(12);
  });

  it("is null for anything that isn't a rate limit or carries no wait", () => {
    expect(retryAfterSecs(quotaExceeded("pages", "user"))).toBeNull();
    expect(retryAfterSecs(rateLimited({}))).toBeNull();
  });
});

describe("notifyFailure", () => {
  it("shows a limit as one keyed warning in place of the generic error", () => {
    const notify = vi.fn();
    notifyFailure(notify, rateLimited(), "Couldn't post comment");
    expect(notify).toHaveBeenCalledExactlyOnceWith(
      "warn",
      "You're doing that too fast. Try again in 7 s.",
      { key: LIMIT_TOAST_KEY },
    );
  });

  it("shows the generic error for any other failure", () => {
    const notify = vi.fn();
    notifyFailure(notify, { status: 500, code: "internal", details: {} }, "Couldn't post comment");
    expect(notify).toHaveBeenCalledExactlyOnceWith("error", "Couldn't post comment");
  });

  it("shows the generic error when there was no answer at all", () => {
    const notify = vi.fn();
    notifyFailure(notify, undefined, "Couldn't post comment");
    expect(notify).toHaveBeenCalledExactlyOnceWith("error", "Couldn't post comment");
  });

  it("stays silent for other failures when there is no generic text", () => {
    const notify = vi.fn();
    notifyFailure(notify, { status: 500, code: "internal", details: {} }, null);
    expect(notify).not.toHaveBeenCalled();
  });
});
