/** Where "Follow the sun" takes the sun to be: a location the user typed,
 *  or else the principal city of the browser's time zone, from a table
 *  generated out of tzdata. A zone the table can't place (`UTC`,
 *  `Etc/GMT+5`) still gets a usable day: on the equator, at the longitude
 *  its UTC offset implies, the sun rises near 06:00 local time and sets
 *  near 18:00. */

import type { LatLon } from "./sun";
import { TZ_COORDS } from "./tzCoords.generated";

/** Where a location came from, so the settings can say. An unplaced zone
 *  is `null` when the browser reported none. */
export type SunLocationSource =
  | { kind: "typed" }
  | { kind: "zone"; zone: string }
  | { kind: "unplaced"; zone: string | null };

export type ResolvedSunLocation = { loc: LatLon; source: SunLocationSource };

/** The browser's IANA time zone, or `null` when it reports an empty one or
 *  `Intl` throws. */
export function browserTimeZone(): string | null {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || null;
  } catch {
    return null;
  }
}

/** The location for a time zone. `offsetMinutes` is what
 *  `Date#getTimezoneOffset()` returns, positive west of Greenwich, and only
 *  counts for a zone the table doesn't hold. */
export function locationForZone(zone: string | null, offsetMinutes: number): ResolvedSunLocation {
  // Own keys only, so a name like `constructor` can't reach Object.prototype.
  if (zone !== null && Object.hasOwn(TZ_COORDS, zone)) {
    const [lat, lon] = TZ_COORDS[zone]!;
    return { loc: { lat, lon }, source: { kind: "zone", zone } };
  }
  // Only zones the table doesn't know get here: Etc/GMT±N, UTC, and zones
  // newer than the bundled tzdata. The earth turns a degree every four
  // minutes, so the offset names a meridian. One past twelve hours lies
  // beyond the antimeridian and wraps round to it from the west (UTC+14 is
  // 150° W), and `+ 0` keeps a -0 out.
  const lon = ((-offsetMinutes / 4 + 540) % 360) - 180 + 0;
  return { loc: { lat: 0, lon }, source: { kind: "unplaced", zone } };
}

/** The location to use at `now`: the typed one if there is one, else the
 *  browser's time zone, re-read each time so a laptop that travels follows
 *  its new zone. `now` picks the UTC offset, which shifts with daylight
 *  saving time. */
export function resolveSunLocation(typed: LatLon | null, now: Date): ResolvedSunLocation {
  if (typed) return { loc: typed, source: { kind: "typed" } };
  return locationForZone(browserTimeZone(), now.getTimezoneOffset());
}

/** A latitude or longitude as typed: decimal degrees, an optional sign,
 *  and `.` or `,` as the decimal separator, within ±`limit`. The minus
 *  sign (`−`, U+2212), which some keyboards and pasted text use, counts as
 *  `-`. Anything else is `null`, exponents (`1e2`) and a second separator
 *  included. */
export function parseCoordinate(raw: string, limit: 90 | 180): number | null {
  const text = raw.trim().replace("\u2212", "-");
  if (!/^[+-]?(?:\d+(?:[.,]\d*)?|[.,]\d+)$/.test(text)) return null;
  const n = Number(text.replace(",", "."));
  return Math.abs(n) <= limit ? n : null;
}

const hemisphere = (degrees: number, positive: string, negative: string) =>
  `${Math.abs(degrees).toFixed(2)}° ${degrees < 0 ? negative : positive}`;

/** A location for reading, e.g. `52.52° N, 13.40° E`. */
export function formatLatLon({ lat, lon }: LatLon): string {
  return `${hemisphere(lat, "N", "S")}, ${hemisphere(lon, "E", "W")}`;
}
