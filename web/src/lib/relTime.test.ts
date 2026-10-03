import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { relTime } from "./relTime";

const NOW = new Date("2026-10-03T12:00:00Z").getTime();
const ago = (ms: number) => new Date(NOW - ms).toISOString();

describe("relTime", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
  });
  afterEach(() => vi.useRealTimers());

  it("says just now for anything under a minute", () => {
    expect(relTime(ago(0))).toBe("just now");
    expect(relTime(ago(59_999))).toBe("just now");
  });

  it("counts whole minutes under an hour", () => {
    expect(relTime(ago(60_000))).toBe("1m ago");
    expect(relTime(ago(59 * 60_000 + 59_999))).toBe("59m ago");
  });

  it("counts whole hours under a day", () => {
    expect(relTime(ago(3_600_000))).toBe("1h ago");
    expect(relTime(ago(23 * 3_600_000 + 3_599_999))).toBe("23h ago");
  });

  it("counts whole days beyond that", () => {
    expect(relTime(ago(86_400_000))).toBe("1d ago");
    expect(relTime(ago(45 * 86_400_000))).toBe("45d ago");
  });

  it("treats a timestamp slightly in the future (clock skew) as just now", () => {
    expect(relTime(new Date(NOW + 5_000).toISOString())).toBe("just now");
  });
});
