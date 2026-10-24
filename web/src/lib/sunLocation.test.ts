import { readFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { afterEach, describe, expect, it, vi } from "vitest";

import { browserTimeZone, formatLatLon, locationForZone, parseCoordinate, resolveSunLocation } from "./sunLocation";
import { TZ_COORDS } from "./tzCoords.generated";

afterEach(() => {
  vi.restoreAllMocks();
});

/** Makes the browser report `timeZone`, leaving the rest of the options real. */
function reportZone(timeZone: string) {
  const real = Intl.DateTimeFormat().resolvedOptions();
  vi.spyOn(Intl.DateTimeFormat.prototype, "resolvedOptions").mockReturnValue({ ...real, timeZone });
}

describe("TZ_COORDS", () => {
  it("places a zone at its principal city", () => {
    const [lat, lon] = TZ_COORDS["Europe/Berlin"]!;
    expect(Math.abs(lat - 52.5)).toBeLessThanOrEqual(0.01);
    expect(Math.abs(lon - 13.37)).toBeLessThanOrEqual(0.01);
  });

  it("gives a legacy link name its target's coordinates", () => {
    expect(TZ_COORDS["Asia/Calcutta"]).toEqual(TZ_COORDS["Asia/Kolkata"]);
  });

  it("covers the legacy names browsers report", () => {
    for (const zone of ["Europe/Kiev", "Asia/Saigon", "America/Buenos_Aires", "US/Eastern"]) {
      expect(TZ_COORDS[zone], zone).toBeDefined();
    }
  });

  it("stays under 10 KB gzipped", () => {
    expect(gzipSync(readFileSync("src/lib/tzCoords.generated.ts")).length).toBeLessThan(10_240);
  });
});

describe("locationForZone", () => {
  it("uses the table for a zone it knows", () => {
    expect(locationForZone("Europe/Berlin", -60)).toEqual({
      loc: { lat: 52.5, lon: 13.37 },
      source: { kind: "zone", zone: "Europe/Berlin" },
    });
  });

  it("puts an unplaced zone on the equator at the longitude its offset implies", () => {
    expect(locationForZone("Etc/GMT-3", -180)).toEqual({
      loc: { lat: 0, lon: 45 },
      source: { kind: "unplaced", zone: "Etc/GMT-3" },
    });
  });

  it("never returns -0 for a zero offset", () => {
    const { loc, source } = locationForZone(null, 0);
    expect(Object.is(loc.lon, 0)).toBe(true);
    expect(source).toEqual({ kind: "unplaced", zone: null });
  });

  it("doesn't take Object.prototype's keys for zones", () => {
    expect(locationForZone("constructor", 0).source).toEqual({ kind: "unplaced", zone: "constructor" });
  });

  it("clamps an offset past the antimeridian", () => {
    // UTC+14 implies 210 degrees east.
    expect(locationForZone("Etc/GMT-14", -840).loc.lon).toBe(180);
  });
});

describe("resolveSunLocation", () => {
  const now = new Date("2026-06-21T12:00:00Z");

  it("prefers a typed location", () => {
    expect(resolveSunLocation({ lat: 1, lon: 2 }, now)).toEqual({
      loc: { lat: 1, lon: 2 },
      source: { kind: "typed" },
    });
  });

  it("falls back to the browser's time zone", () => {
    reportZone("Asia/Calcutta");
    const [lat, lon] = TZ_COORDS["Asia/Calcutta"]!;
    expect(resolveSunLocation(null, now)).toEqual({
      loc: { lat, lon },
      source: { kind: "zone", zone: "Asia/Calcutta" },
    });
  });
});

describe("browserTimeZone", () => {
  it("returns null when Intl throws", () => {
    vi.spyOn(Intl, "DateTimeFormat").mockImplementation(() => {
      throw new RangeError("no time zone data");
    });
    expect(browserTimeZone()).toBeNull();
  });

  it("returns null when the browser reports no zone", () => {
    reportZone("");
    expect(browserTimeZone()).toBeNull();
  });
});

describe("parseCoordinate", () => {
  it("reads decimal degrees with either separator and an optional sign", () => {
    expect(parseCoordinate("52.52", 90)).toBe(52.52);
    expect(parseCoordinate(" 52,52 ", 90)).toBe(52.52);
    expect(parseCoordinate("+13.4", 180)).toBe(13.4);
    expect(parseCoordinate("-90", 90)).toBe(-90);
  });

  it("rejects anything that isn't a plain number within the limit", () => {
    for (const raw of ["1e2", "", "abc", "1.2.3"]) {
      expect(parseCoordinate(raw, 180), JSON.stringify(raw)).toBeNull();
    }
    expect(parseCoordinate("91", 90)).toBeNull();
    expect(parseCoordinate("-181", 180)).toBeNull();
  });
});

describe("formatLatLon", () => {
  it("names the hemispheres instead of signing the degrees", () => {
    expect(formatLatLon({ lat: -33.87, lon: -70.5 })).toBe("33.87° S, 70.50° W");
    expect(formatLatLon({ lat: 52.52, lon: 13.4 })).toBe("52.52° N, 13.40° E");
  });
});
