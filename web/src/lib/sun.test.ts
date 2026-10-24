import { describe, expect, it } from "vitest";

import { SUNRISE_ALTITUDE, isDaytime, solarElevation, sunDay, type SunDay } from "./sun";

const berlin = { lat: 52.52, lon: 13.405 };
const sydney = { lat: -33.8688, lon: 151.2093 };
const tromso = { lat: 69.6496, lon: 18.956 };
const newYork = { lat: 40.7128, lon: -74.006 };

const at = (iso: string) => new Date(iso);

/** Within two minutes of a published sunrise/sunset table, which only
 *  gives whole minutes anyway. */
function expectNear(actual: Date, expected: string) {
  const diff = Math.abs(actual.getTime() - Date.parse(expected));
  expect(diff, `${actual.toISOString()} vs ${expected}`).toBeLessThanOrEqual(120_000);
}

function expectTimes(day: SunDay, sunrise: string, sunset: string) {
  expect(day.kind).toBe("normal");
  if (day.kind !== "normal") return;
  expectNear(day.sunrise, sunrise);
  expectNear(day.sunset, sunset);
}

describe("sunDay", () => {
  it("matches Berlin's summer solstice", () => {
    expectTimes(sunDay(at("2026-06-21T12:00:00Z"), berlin), "2026-06-21T02:43:00Z", "2026-06-21T19:33:00Z");
  });

  it("matches Berlin's winter solstice", () => {
    expectTimes(sunDay(at("2026-12-21T12:00:00Z"), berlin), "2026-12-21T07:15:00Z", "2026-12-21T14:54:00Z");
  });

  it("matches Berlin's spring equinox", () => {
    // USNO Astronomical Applications, 2026-03-20 at 52.52 N 13.405 E.
    expectTimes(sunDay(at("2026-03-20T12:00:00Z"), berlin), "2026-03-20T05:09:00Z", "2026-03-20T17:19:00Z");
  });

  it("matches Sydney's winter solstice, whose sunrise falls on the previous UTC date", () => {
    expectTimes(sunDay(at("2026-06-21T02:00:00Z"), sydney), "2026-06-20T21:00:00Z", "2026-06-21T06:54:00Z");
  });

  it("picks the solar day whose noon is nearest", () => {
    const early = sunDay(at("2026-06-22T01:00:00Z"), berlin);
    expect(early.kind).toBe("normal");
    if (early.kind === "normal") expectNear(early.sunrise, "2026-06-22T02:43:00Z"); // not the 21st

    // Berlin's noon is around 11:07Z, so late on the 21st the 22nd is nearer.
    expect(sunDay(at("2026-06-21T23:30:00Z"), berlin)).toEqual(sunDay(at("2026-06-22T11:00:00Z"), berlin));
    // New York's noon is around 16:58Z, so early on the 21st the 20th is nearer.
    expect(sunDay(at("2026-06-21T03:00:00Z"), newYork)).toEqual(sunDay(at("2026-06-20T17:00:00Z"), newYork));
  });

  it("reports polar day and polar night", () => {
    expect(sunDay(at("2026-06-21T12:00:00Z"), tromso)).toEqual({ kind: "polar-day" });
    expect(sunDay(at("2026-12-21T12:00:00Z"), tromso)).toEqual({ kind: "polar-night" });
  });

  it("puts the sun at the sunrise altitude at its own sunrise and sunset", () => {
    for (const [around, loc] of [
      ["2026-06-21T12:00:00Z", berlin],
      ["2026-12-21T12:00:00Z", berlin],
      ["2026-06-21T02:00:00Z", sydney],
    ] as const) {
      const day = sunDay(at(around), loc);
      expect(day.kind).toBe("normal");
      if (day.kind !== "normal") continue;
      expect(solarElevation(day.sunrise, loc)).toBeCloseTo(SUNRISE_ALTITUDE, 2);
      expect(solarElevation(day.sunset, loc)).toBeCloseTo(SUNRISE_ALTITUDE, 2);
    }
  });
});

describe("solarElevation", () => {
  it("computes elevation", () => {
    expect(Math.abs(solarElevation(at("2026-06-21T11:00:00Z"), berlin) - 60.88)).toBeLessThanOrEqual(0.05);
  });
});

describe("isDaytime", () => {
  it("knows day from night", () => {
    expect(isDaytime(at("2026-06-21T11:00:00Z"), berlin)).toBe(true);
    expect(isDaytime(at("2026-06-21T23:00:00Z"), tromso)).toBe(true); // midnight sun

    expect(isDaytime(at("2026-06-21T21:00:00Z"), berlin)).toBe(false);
    expect(isDaytime(at("2026-12-21T15:30:00Z"), berlin)).toBe(false);
    expect(isDaytime(at("2026-12-21T11:00:00Z"), tromso)).toBe(false); // polar night at noon
  });
});
