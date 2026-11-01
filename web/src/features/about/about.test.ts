import { describe, expect, it } from "vitest";

import { formatReleaseDate, missedReleasesText, shortCommit } from "./about";

describe("missedReleasesText", () => {
  it("counts the releases since the last visit", () => {
    expect(missedReleasesText(2)).toBe("2 releases since your last visit");
    expect(missedReleasesText(5)).toBe("5 releases since your last visit");
  });

  it("calls a single one a new release", () => {
    expect(missedReleasesText(1)).toBe("1 new release since your last visit");
  });
});

describe("shortCommit", () => {
  it("keeps the first seven characters of a full hash", () => {
    expect(shortCommit("52fa7a1c0ffee0123456789abcdef0123456789a")).toBe("52fa7a1");
  });

  it("leaves a hash that is already short alone", () => {
    expect(shortCommit("52fa7a1")).toBe("52fa7a1");
  });
});

describe("formatReleaseDate", () => {
  it("formats the changelog's calendar date without shifting it into the local zone", () => {
    // Pinned to UTC on both sides: "2026-10-21" is a calendar date, and a
    // local-zone format would show the 20th anywhere west of Greenwich.
    expect(formatReleaseDate("2026-10-21")).toBe(
      new Date(Date.UTC(2026, 9, 21)).toLocaleDateString(undefined, {
        day: "numeric",
        month: "short",
        year: "numeric",
        timeZone: "UTC",
      }),
    );
  });
});
