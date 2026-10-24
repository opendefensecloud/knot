/** Where the sun is, and when it rises and sets, for "Follow the sun".
 *
 *  The equations are the NOAA Solar Calculator's, after Meeus'
 *  "Astronomical Algorithms". NOAA's shorter Fourier-series approximation
 *  is off by up to three minutes at mid latitudes, enough for the times
 *  knot shows to disagree with a weather app's; these land within a
 *  minute of published tables. Everything here is pure: instants in,
 *  angles and instants out, no clock and no time zone. */

/** Degrees; `lon` is positive east of Greenwich. */
export type LatLon = { lat: number; lon: number };

/** The elevation of the sun's centre at sunrise and sunset. Its upper edge
 *  touches the horizon there once refraction (about 0.567°) and its radius
 *  (about 0.266°) are counted, which is how published tables define it. */
export const SUNRISE_ALTITUDE = -0.833;

/** A solar day: its sunrise and sunset, or neither when the sun stays up
 *  (polar day) or down (polar night) all day. */
export type SunDay =
  | { kind: "normal"; sunrise: Date; sunset: Date }
  | { kind: "polar-day" }
  | { kind: "polar-night" };

type Polar = Exclude<SunDay, { kind: "normal" }>;

const MINUTE_MS = 60_000;
const DAY_MS = 86_400_000;

// Trigonometry in degrees, which is how all the equations are written.
const rad = (d: number) => (d * Math.PI) / 180;
const deg = (r: number) => (r * 180) / Math.PI;
const sin = (d: number) => Math.sin(rad(d));
const cos = (d: number) => Math.cos(rad(d));
const tan = (d: number) => Math.tan(rad(d));

/** The sun's declination (degrees) and the equation of time (minutes,
 *  apparent minus mean solar time) at an instant. */
function solar(atMs: number): { decl: number; eqtime: number } {
  const T = (atMs / DAY_MS + 2440587.5 - 2451545) / 36525; // Julian centuries since J2000
  const L0 = (280.46646 + T * (36000.76983 + T * 0.0003032)) % 360; // geometric mean longitude
  const M = 357.52911 + T * (35999.05029 - 0.0001537 * T); // mean anomaly
  const e = 0.016708634 - T * (0.000042037 + 0.0000001267 * T); // orbital eccentricity
  const C =
    sin(M) * (1.914602 - T * (0.004817 + 0.000014 * T)) +
    sin(2 * M) * (0.019993 - 0.000101 * T) +
    sin(3 * M) * 0.000289; // equation of centre
  const omega = 125.04 - 1934.136 * T; // the moon's ascending node, for nutation
  const lambda = L0 + C - 0.00569 - 0.00478 * sin(omega); // apparent longitude
  // Obliquity of the ecliptic, corrected for nutation.
  const eps = 23 + (26 + (21.448 - T * (46.815 + T * (0.00059 - T * 0.001813))) / 60) / 60 + 0.00256 * cos(omega);
  const decl = deg(Math.asin(sin(eps) * sin(lambda)));
  const y = tan(eps / 2) ** 2;
  const eqtimeRad =
    y * sin(2 * L0) -
    2 * e * sin(M) +
    4 * e * y * sin(M) * cos(2 * L0) -
    0.5 * y * y * sin(4 * L0) -
    1.25 * e * e * sin(2 * M);
  return { decl, eqtime: 4 * deg(eqtimeRad) }; // the earth turns a degree every four minutes
}

const utcMidnight = (ms: number) => Math.floor(ms / DAY_MS) * DAY_MS;

/** How high the sun's centre stands above the horizon, in degrees, as
 *  geometry alone: no refraction, which `SUNRISE_ALTITUDE` already folds
 *  in. Negative below the horizon. */
export function solarElevation(at: Date, loc: LatLon): number {
  const ms = at.getTime();
  const { decl, eqtime } = solar(ms);
  const minutes = (ms - utcMidnight(ms)) / MINUTE_MS;
  const hourAngle = (minutes + eqtime + 4 * loc.lon) / 4 - 180;
  const cosZenith = sin(loc.lat) * sin(decl) + cos(loc.lat) * cos(decl) * cos(hourAngle);
  return 90 - deg(Math.acos(Math.min(1, Math.max(-1, cosZenith))));
}

/** Whether the sun is up. Polar day and polar night need no special case:
 *  the sun simply never crosses the line. */
export function isDaytime(at: Date, loc: LatLon): boolean {
  return solarElevation(at, loc) > SUNRISE_ALTITUDE;
}

/** Solar noon of the UTC day starting at `day`. The equation of time
 *  depends on the instant, so it is re-evaluated at each estimate. */
function solarNoon(day: number, lon: number): number {
  let t = day + (720 - 4 * lon) * MINUTE_MS; // mean noon
  for (let i = 0; i < 2; i++) t = day + (720 - 4 * lon - solar(t).eqtime) * MINUTE_MS;
  return t;
}

/** The sunrise or sunset of the solar day whose noon is `noon`, refined
 *  from noon outwards because the declination drifts over the day. A sun
 *  that never reaches the sunrise altitude makes the whole day polar. */
function sunEvent(day: number, noon: number, loc: LatLon, which: "sunrise" | "sunset"): number | Polar {
  let t = noon;
  for (let i = 0; i < 4; i++) {
    const { decl, eqtime } = solar(t);
    // sin(SUNRISE_ALTITUDE) is cos(90.833°), 90.833° being the zenith angle at the event.
    const cosH = sin(SUNRISE_ALTITUDE) / (cos(loc.lat) * cos(decl)) - tan(loc.lat) * tan(decl);
    if (cosH > 1) return { kind: "polar-night" };
    if (cosH < -1) return { kind: "polar-day" };
    const H = deg(Math.acos(cosH));
    t = day + (720 - 4 * (loc.lon + (which === "sunrise" ? H : -H)) - eqtime) * MINUTE_MS;
  }
  return t;
}

/** The sunrise and sunset of the solar day whose noon is nearest `around`.
 *  Nearest noon rather than the UTC date, because a UTC date cuts some
 *  places' days in two: Sydney's winter sunrise falls on the previous UTC
 *  date, New York's summer sunset on the next one. Either event may land
 *  on a different date from `around`. */
export function sunDay(around: Date, loc: LatLon): SunDay {
  const at = around.getTime();
  let day = utcMidnight(at);
  let noon = solarNoon(day, loc.lon);
  // Noon always lies within about a day of `around`, so one step suffices.
  if (Math.abs(noon - at) > DAY_MS / 2) {
    day += noon > at ? -DAY_MS : DAY_MS;
    noon = solarNoon(day, loc.lon);
  }
  const sunrise = sunEvent(day, noon, loc, "sunrise");
  if (typeof sunrise !== "number") return sunrise;
  const sunset = sunEvent(day, noon, loc, "sunset");
  if (typeof sunset !== "number") return sunset;
  return { kind: "normal", sunrise: new Date(sunrise), sunset: new Date(sunset) };
}
