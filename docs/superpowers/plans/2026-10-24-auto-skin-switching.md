# Automatic Skin Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user choose "Follow system" or "Follow the sun" so knot switches between a chosen light skin and a chosen dark skin on its own.

**Architecture:** Pure modules for solar math (`lib/sun.ts`), time-zone → coordinates (`lib/sunLocation.ts` over a generated tzdata table) and skin preferences (`styles/skinPrefs.ts`) feed the zustand store in `stores/ui.ts`, which now tracks the mode, the two slot skins and the skin *showing*. A small controller (`stores/skinWatcher.ts`), started from `main.tsx`, re-resolves the showing skin on OS appearance changes, a 60 s tick in sun mode, focus/visibility/pageshow, and other tabs' storage writes. The Settings page's existing card grid gains a mode switch and a sun/location panel.

**Tech Stack:** React 19, TypeScript, zustand 5, Tailwind 3 (existing tokens), Vitest + Testing Library (jsdom), Playwright.

**Spec:** `docs/superpowers/specs/2026-10-24-auto-skin-switching-design.md`

## Global Constraints

- Frontend only (`web/`, plus the generator in `tools/` and an e2e flow). No server, API or migration change. No new runtime dependency.
- Every `localStorage` read and write is wrapped in `try`/`catch` and degrades to defaults / session-only, as in `web/src/stores/ui.ts` today.
- Keys: `knot.skin` (hand-picked, unchanged meaning), `knot.skinMode` (`manual` | `system` | `sun`, default `manual`), `knot.skinLight` (light-scheme id, default `light`), `knot.skinDark` (dark-scheme id, default `dark`), `knot.sunLocation` (JSON `{"lat","lon"}`, absent = from time zone).
- Daytime = solar elevation > −0.833°.
- No network request, no geolocation API, no location prompt.
- Store field `skin` = the skin **showing**; `theme` = its scheme. `<html data-skin data-theme>` keep their meaning.
- Existing tests (`ui.skin.test.ts`, `SkinPicker.test.tsx`, `AppShell` tests, `e2e/flows/skin.spec.ts`) keep passing without changing what they assert.
- Commit style: Conventional Commits (`feat(web): …`, `test(e2e): …`, `docs: …`), ending with the session's `Co-Authored-By` trailer.
- Match surrounding style: doc comments explain *why*, two-space indent, double quotes, named exports, tests next to code.

## Review Focus

1. **Storage that throws or is empty** (private window, blocked site data): every read gives defaults, every setter still applies the skin for the session, nothing throws. Tests in Task 3 and Task 4.
2. **No `matchMedia`, or an OS appearance change while knot is open:** without `matchMedia` system mode shows the light slot. A change event flips the skin without a reload. Tests in Task 3 and Task 5.
3. **Laptop wakes after sunset (timers didn't fire) or the clock jumps:** a `focus` / `visibilitychange` / `pageshow` event re-resolves at once. Test in Task 5.
4. **Another tab clears or rewrites storage** (`storage` event with `key === null`, or a garbage value): this tab adopts the validated values or the defaults and never writes back. Tests in Task 5.
5. **Hand-typed coordinates:** `52,52`, surrounding spaces, `+13.4`, `1e2`, empty, `91`, `-181` and `abc` are each accepted or rejected predictably. The stored value is rounded to 2 decimals. Garbage already stored in `knot.sunLocation` counts as "no typed location". Tests in Task 3 and Task 6.

---

### Task 1: Solar math

**Files:**
- Create: `web/src/lib/sun.ts`
- Test: `web/src/lib/sun.test.ts`

**Interfaces:**
- Produces:
  - `export type LatLon = { lat: number; lon: number }` (degrees; lon positive east)
  - `export const SUNRISE_ALTITUDE = -0.833`
  - `export function solarElevation(at: Date, loc: LatLon): number` (degrees, no refraction)
  - `export function isDaytime(at: Date, loc: LatLon): boolean` (`solarElevation > SUNRISE_ALTITUDE`)
  - `export type SunDay = { kind: "normal"; sunrise: Date; sunset: Date } | { kind: "polar-day" } | { kind: "polar-night" }`
  - `export function sunDay(around: Date, loc: LatLon): SunDay`: the sunrise and sunset of the solar day whose solar noon is nearest to `around`

- [ ] **Step 1: Write the failing tests** in `web/src/lib/sun.test.ts`. Reference times are published-table values. Assert each within ±2 min (`Math.abs(actual - expected) <= 120_000`).

```ts
const berlin = { lat: 52.52, lon: 13.405 };
const sydney = { lat: -33.8688, lon: 151.2093 };
const tromso = { lat: 69.6496, lon: 18.956 };

it("matches Berlin's summer solstice", () => {
  // sunDay(new Date("2026-06-21T12:00:00Z"), berlin) → normal,
  // sunrise ≈ 2026-06-21T02:43:00Z, sunset ≈ 2026-06-21T19:33:00Z
});
it("matches Berlin's winter solstice", () => {
  // around 2026-12-21T12:00:00Z → sunrise ≈ 07:15Z, sunset ≈ 14:54Z (same date)
});
it("matches Sydney's winter solstice, whose sunrise falls on the previous UTC date", () => {
  // around 2026-06-21T02:00:00Z → sunrise ≈ 2026-06-20T21:00:00Z, sunset ≈ 2026-06-21T06:54:00Z
});
it("picks the solar day whose noon is nearest", () => {
  // around 2026-06-22T01:00:00Z, berlin → sunrise ≈ 2026-06-22T02:43:00Z (not the 21st)
});
it("reports polar day and polar night", () => {
  // tromso around 2026-06-21T12:00:00Z → { kind: "polar-day" }
  // tromso around 2026-12-21T12:00:00Z → { kind: "polar-night" }
});
it("computes elevation", () => {
  // solarElevation(new Date("2026-06-21T11:00:00Z"), berlin) within 0.05 of 60.88
});
it("knows day from night", () => {
  // isDaytime true:  berlin 2026-06-21T11:00:00Z; tromso 2026-06-21T23:00:00Z (midnight sun)
  // isDaytime false: berlin 2026-06-21T21:00:00Z; berlin 2026-12-21T15:30:00Z; tromso 2026-12-21T11:00:00Z
});
```

- [ ] **Step 2: Run, expect FAIL** (module missing): `cd web && pnpm vitest run src/lib/sun.test.ts`

- [ ] **Step 3: Implement `web/src/lib/sun.ts`** with the NOAA Solar Calculator (Meeus) equations. The simpler NOAA Fourier-series approximation is off by up to 3 min at mid latitudes, so don't use it. The core, which the tests don't pin down:

```ts
// Declination (radians) and equation of time (minutes) at an instant.
function solar(atMs: number): { decl: number; eqtime: number } {
  const T = (atMs / 86_400_000 + 2440587.5 - 2451545) / 36525; // Julian century
  const L0 = (280.46646 + T * (36000.76983 + T * 0.0003032)) % 360;
  const M = 357.52911 + T * (35999.05029 - 0.0001537 * T);
  const e = 0.016708634 - T * (0.000042037 + 0.0000001267 * T);
  const C = sin(M) * (1.914602 - T * (0.004817 + 0.000014 * T)) + sin(2 * M) * (0.019993 - 0.000101 * T) + sin(3 * M) * 0.000289;
  const omega = 125.04 - 1934.136 * T;
  const lambda = L0 + C - 0.00569 - 0.00478 * sin(omega);
  const eps = 23 + (26 + (21.448 - T * (46.815 + T * (0.00059 - T * 0.001813))) / 60) / 60 + 0.00256 * cos(omega);
  const decl = Math.asin(sin(eps) * sin(lambda));
  const y = Math.tan(rad(eps / 2)) ** 2;
  const eqtime = 4 * deg(y * sin(2 * L0) - 2 * e * sin(M) + 4 * e * y * sin(M) * cos(2 * L0) - 0.5 * y * y * sin(4 * L0) - 1.25 * e * e * sin(2 * M));
  return { decl, eqtime };
}
// (sin/cos take degrees here; rad/deg convert.)
```

- Elevation: hour angle `ha = (minutesSinceUtcMidnight + eqtime + 4 * lon) / 4 - 180` (degrees); `cosZ = sin(lat)·sin(decl) + cos(lat)·cos(decl)·cos(ha)`, clamped to [−1, 1]; elevation is `90 − acos(cosZ)`.
- `sunDay`: let `day` be the UTC midnight of `around`. Solar noon on that day is `day + (720 − 4·lon − eqtime(noon))` minutes; iterate twice. If that noon is more than 12 h away from `around`, step `day` by one day and recompute. For each event, start at noon and iterate 4 times: `cosH = cos(90.833°)/(cos lat·cos decl) − tan lat·tan decl`, evaluated at the current estimate. `cosH > 1` means polar night and `cosH < −1` means polar day. Otherwise `t = day + (720 − 4·(lon ∓ H) − eqtime)` minutes, with `+H` for sunrise and `−H` for sunset. (A scratch prototype of exactly this lands within 20 s of published Berlin tables and reaches −0.833° at its own event times.)

- [ ] **Step 4: Run, expect PASS:** `cd web && pnpm vitest run src/lib/sun.test.ts`

- [ ] **Step 5: Commit:** `git add web/src/lib/sun.ts web/src/lib/sun.test.ts && git commit -m "feat(web): solar elevation and sunrise/sunset math"`

---

### Task 2: Time zone → location

**Files:**
- Create: `tools/gen-tz-coords.mjs`
- Create (generated): `web/src/lib/tzCoords.generated.ts`
- Create: `web/src/lib/sunLocation.ts`
- Modify: `Makefile` (add `tz.gen` next to `schema.gen`)
- Test: `web/src/lib/sunLocation.test.ts`

**Interfaces:**
- Consumes: `LatLon` from `web/src/lib/sun.ts`
- Produces:
  - `export const TZDATA_VERSION: string` and `export const TZ_COORDS: Readonly<Record<string, readonly [number, number]>>` (lat, lon, 2 decimals) in `tzCoords.generated.ts`
  - `export type SunLocationSource = { kind: "typed" } | { kind: "zone"; zone: string } | { kind: "unplaced"; zone: string | null }`
  - `export type ResolvedSunLocation = { loc: LatLon; source: SunLocationSource }`
  - `export function browserTimeZone(): string | null`: `Intl.DateTimeFormat().resolvedOptions().timeZone`; `null` if empty or it throws
  - `export function locationForZone(zone: string | null, offsetMinutes: number): ResolvedSunLocation`: `offsetMinutes` is `Date#getTimezoneOffset()` (positive west)
  - `export function resolveSunLocation(typed: LatLon | null, now: Date): ResolvedSunLocation`: typed wins; otherwise `locationForZone(browserTimeZone(), now.getTimezoneOffset())`

- [ ] **Step 1: Write the generator** `tools/gen-tz-coords.mjs <extracted-tzdata-dir>`. It reads `zone.tab`, whose coordinates are `±DDMM[SS]±DDDMM[SS]`, converted to decimal degrees and rounded to 2 decimals. It reads `backward` (`Link TARGET NAME` lines), resolves link chains transitively, gives each link name its target's coordinates, and skips targets without coordinates. It writes `web/src/lib/tzCoords.generated.ts` sorted by name, with a header saying the file is generated, naming the generator and the tzdata version (from the `version` file), and noting that tzdata is public domain. Add `tz.gen: ## regenerate web/src/lib/tzCoords.generated.ts (TZDATA=<extracted tzdata dir>)` to the Makefile. Run the generator against tzdata 2026e and commit the output. It should hold about 549 zones. Fetch `https://data.iana.org/time-zones/releases/tzdata2026e.tar.gz` into a temp dir outside the repo.

- [ ] **Step 2: Write the failing tests** in `web/src/lib/sunLocation.test.ts`:
  - `TZ_COORDS["Europe/Berlin"]` within 0.01 of `[52.5, 13.37]`; `TZ_COORDS["Asia/Kolkata"]` equals `TZ_COORDS["Asia/Calcutta"]`; `"Europe/Kiev"`, `"Asia/Saigon"`, `"America/Buenos_Aires"` and `"US/Eastern"` are present.
  - `locationForZone("Europe/Berlin", -60)` → `{ loc: {lat: 52.5, lon: 13.37}, source: { kind: "zone", zone: "Europe/Berlin" } }`
  - `locationForZone("Etc/GMT-3", -180)` → `{ loc: { lat: 0, lon: 45 }, source: { kind: "unplaced", zone: "Etc/GMT-3" } }`; `locationForZone(null, 0)` → `lon` is `0`, not `-0` (assert with `Object.is`), with source `{ kind: "unplaced", zone: null }`; `locationForZone("Etc/GMT-14", -840)` → `lon` clamped to `180` (210 unclamped).
  - `resolveSunLocation({ lat: 1, lon: 2 }, now)` → `{ loc: { lat: 1, lon: 2 }, source: { kind: "typed" } }`.
  - `browserTimeZone()` returns `null` when `Intl.DateTimeFormat` throws (spy with `vi.spyOn(Intl, "DateTimeFormat")`).
  - Size budget: `zlib.gzipSync(fs.readFileSync("src/lib/tzCoords.generated.ts")).length < 10_240`.

- [ ] **Step 3: Run, expect FAIL:** `cd web && pnpm vitest run src/lib/sunLocation.test.ts`

- [ ] **Step 4: Implement `web/src/lib/sunLocation.ts`.** For an unplaced zone, `lon = clamp(-offsetMinutes / 4, -180, 180)`, normalised so it is never `-0`, and `lat = 0`.

- [ ] **Step 5: Run, expect PASS.** Then run `pnpm tsc` and `pnpm lint`. The generated file must pass lint. If it doesn't, add it to the ESLint ignores rather than hand-editing it.

- [ ] **Step 6: Commit:** `feat(web): locate the browser's time zone from bundled tzdata`

---

### Task 3: Skin preferences

**Files:**
- Modify: `web/src/styles/skins.ts` (add slot defaults + scheme check)
- Create: `web/src/styles/skinPrefs.ts`
- Test: `web/src/styles/skinPrefs.test.ts`

**Interfaces:**
- Consumes: `LatLon`, `isDaytime` (Task 1); `resolveSunLocation` (Task 2); `findSkin`, `DEFAULT_SKIN_ID`, `Scheme` (existing)
- Produces, in `skins.ts`: `DEFAULT_LIGHT_SKIN_ID = "light"`, `DEFAULT_DARK_SKIN_ID = "dark"`, `isSkinOfScheme(id: string | null | undefined, scheme: Scheme): boolean`
- Produces, in `skinPrefs.ts`:
  - `export type SkinMode = "manual" | "system" | "sun"`
  - `export const SKIN_KEYS = { manual: "knot.skin", legacyTheme: "knot.theme", mode: "knot.skinMode", light: "knot.skinLight", dark: "knot.skinDark", location: "knot.sunLocation" } as const`
  - `export type SkinPrefs = { mode: SkinMode; manual: string; light: string; dark: string; location: LatLon | null }`
  - `parseSkinMode(raw: string | null): SkinMode`; `parseSlot(raw: string | null, scheme: Scheme): string`; `parseSunLocation(raw: string | null): LatLon | null` (JSON object with finite numbers in range, else `null`)
  - `readManualSkin(): string` (moved verbatim from `ui.ts`'s `readInitialSkin`, including the legacy `knot.theme` fallback)
  - `readSkinPrefs(): SkinPrefs`
  - `hasStoredSlot(scheme: Scheme): boolean` (the stored key holds a valid skin of that scheme)
  - `writePref(key: string, value: string | null): void` (`null` removes; swallows storage errors)
  - `resolveSkin(prefs: SkinPrefs, env: { systemDark: boolean; daytime: boolean }): string` (pure)
  - `systemPrefersDark(): boolean` (`false` when `window.matchMedia` isn't a function)
  - `currentSkin(prefs: SkinPrefs, now?: Date): string` (impure: `resolveSkin` with `systemPrefersDark()` and `isDaytime(now, resolveSunLocation(prefs.location, now).loc)`)

- [ ] **Step 1: Write the failing tests:**
  - `resolveSkin`: manual → `prefs.manual` whatever the env; system → `dark` slot iff `systemDark`; sun → `light` slot iff `daytime`.
  - `parseSkinMode`: `"system"`/`"sun"`/`"manual"` pass, and `null`, `""` and `"auto"` give `"manual"`.
  - `parseSlot`: `("paper","light")` → `"paper"`; `("nord","light")` → `"light"`; `("paper","dark")` → `"dark"`; `(null,"dark")` → `"dark"`; `("nope","light")` → `"light"`.
  - `parseSunLocation`: `'{"lat":52.52,"lon":13.4}'` passes. `null`, `"x"`, `'null'`, `'{"lat":"1","lon":2}'`, `'{"lat":91,"lon":0}'`, `'{"lat":0,"lon":-181}'` and `'{"lat":1}'` each give `null`.
  - `readSkinPrefs()` with empty storage → `{ mode: "manual", manual: "light", light: "light", dark: "dark", location: null }`. When `Storage.prototype.getItem` throws (`vi.spyOn`), it returns the same defaults.
  - `hasStoredSlot("light")` is false for no key and for `knot.skinLight = "nord"`, true for `"paper"`.
  - `systemPrefersDark()` is false when `window.matchMedia` is undefined, and true when a stub returns `{ matches: true }`.
  - `currentSkin` in sun mode with `location: { lat: 52.52, lon: 13.405 }` → the light slot at `2026-06-21T11:00:00Z` and the dark slot at `2026-06-21T21:00:00Z`.
  - `writePref` swallows a throwing `setItem`.

- [ ] **Step 2: Run, expect FAIL:** `cd web && pnpm vitest run src/styles/skinPrefs.test.ts`

- [ ] **Step 3: Implement** `skins.ts` additions and `skinPrefs.ts`. Keep `ui.ts` compiling: its `readInitialSkin` becomes `export const readInitialSkin = readManualSkin;` (Task 4 depends on that name staying exported).

- [ ] **Step 4: Run, expect PASS,** plus the whole web suite: `cd web && pnpm test && pnpm tsc && pnpm lint`

- [ ] **Step 5: Commit:** `feat(web): skin switching preferences and resolver`

---

### Task 4: Store: mode, slots and the skin showing

**Files:**
- Modify: `web/src/stores/ui.ts`
- Modify: `web/src/main.tsx` (pre-paint stamp)
- Test: `web/src/stores/ui.skinMode.test.ts` (new; leave `ui.skin.test.ts` as is)

**Interfaces:**
- Consumes: everything Task 3 produces
- Produces (on `useUi`):
  - state: `skin: string` (showing), `theme: Theme`, `skinMode: SkinMode`, `manualSkin: string`, `lightSkin: string`, `darkSkin: string`, `sunLocation: LatLon | null`
  - `setSkin(id: string): void`: pick by hand. `manualSkin = known(id) ? id : DEFAULT_SKIN_ID`, `skinMode = "manual"`. Persists `knot.skin` and `knot.skinMode`, then refreshes.
  - `setSkinMode(mode: SkinMode): void`: no-op if unchanged. Into `"manual"`: `manualSkin` becomes the skin showing now, and `knot.skin` is persisted. Into `"system"`/`"sun"`: if `!hasStoredSlot(scheme)` for the scheme of the skin showing now, seed that slot with it and persist it. Then persist the mode and refresh.
  - `setLightSkin(id: string)` / `setDarkSkin(id: string)`: ignored unless `isSkinOfScheme(id, "light"/"dark")`. Persist the slot key, then refresh.
  - `setSunLocation(loc: LatLon | null): void`: rounds lat/lon to 2 decimals. Persists JSON, or removes the key for `null`. Then refreshes.
  - `refreshSkin(now?: Date): void`: `next = currentSkin(prefsFromState, now)`. When `next !== skin`, it stamps and sets `skin`/`theme`. It never writes storage.
  - `adoptStoredSkinPrefs(): void`: `readSkinPrefs()` into state with no writes, then `refreshSkin()`.
  - module exports: `readInitialSkin` (kept), `stampSkin` (kept), `resolveInitialSkin(): string` (`currentSkin(readSkinPrefs())`)

- [ ] **Step 1: Write the failing tests** (`beforeEach`: `localStorage.clear()`, `useUi.getState().setSkin("light")`; `afterEach`: restore spies, `vi.useRealTimers()`):
  - `setSkin("nord")` → `skinMode` `"manual"`, `skin` `"nord"`, `localStorage["knot.skinMode"] === "manual"`.
  - Seeding: `setSkin("nord"); setSkinMode("sun")` → `darkSkin === "nord"`, `localStorage["knot.skinDark"] === "nord"`, `lightSkin === "light"`, and `knot.skinLight` is absent. With `knot.skinDark = "dracula"` stored and adopted first, `setSkinMode("system")` from Nord leaves `darkSkin === "dracula"`.
  - Into manual: in system mode with the light slot set to `"paper"` and no `matchMedia` (light), `setSkinMode("manual")` → `manualSkin === "paper"` and `skin === "paper"`, so nothing jumps.
  - `setLightSkin("nord")` is ignored; `setLightSkin("sage")` sets `lightSkin` and `knot.skinLight`, and in system mode with a light OS also sets `skin === "sage"` and `data-skin="sage"`.
  - `setSunLocation({ lat: 52.51963, lon: 13.40495 })` stores `{"lat":52.52,"lon":13.4}`; `setSunLocation(null)` removes the key.
  - Sun mode with the Berlin location: `vi.useFakeTimers(); vi.setSystemTime(new Date("2026-06-21T21:00:00Z")); refreshSkin()` → the dark slot showing and `data-theme="dark"`.
  - `refreshSkin` doesn't touch `<html>` when the result is unchanged (set `data-skin` to a sentinel, call it, and the sentinel survives).
  - `adoptStoredSkinPrefs()` after writing `knot.skinMode = "system"` and `knot.skinDark = "nord"` straight to storage with a dark `matchMedia` stub → `skinMode === "system"` and `skin === "nord"`, and `setItem` is never called (spy).
  - Throwing storage (`setItem` spy throws): `setSkinMode("system")` still switches `skinMode` and applies the skin.
  - `resolveInitialSkin()` with stored sun mode at night returns the dark slot.

- [ ] **Step 2: Run, expect FAIL:** `cd web && pnpm vitest run src/stores/ui.skinMode.test.ts`

- [ ] **Step 3: Implement** the store changes. Initial state is computed from one `readSkinPrefs()` call, and `skin` from `currentSkin` on those prefs. In `main.tsx`, replace `stampSkin(readInitialSkin())` with `stampSkin(resolveInitialSkin())` and update its comment to say the stamp is the resolved skin, so a sun-mode user opening a laptop after dark gets dark at first paint.

- [ ] **Step 4: Run, expect PASS,** and the whole suite: `cd web && pnpm test && pnpm tsc && pnpm lint`. `ui.skin.test.ts`, `SkinPicker.test.tsx` and `AppShell.test.tsx` must pass unchanged.

- [ ] **Step 5: Commit:** `feat(web): store tracks skin mode, light/dark slots and the skin showing`

---

### Task 5: Live switching controller

**Files:**
- Create: `web/src/stores/skinWatcher.ts`
- Modify: `web/src/main.tsx` (start it once, after the stamps)
- Test: `web/src/stores/skinWatcher.test.ts`

**Interfaces:**
- Consumes: `useUi` (`refreshSkin`, `adoptStoredSkinPrefs`, `skinMode`) from Task 4; `SKIN_KEYS` from Task 3
- Produces: `export const SUN_RECHECK_MS = 60_000`; `export function watchSkinEnvironment(): () => void` (returns a cleanup that removes every listener, the interval and the store subscription)

- [ ] **Step 1: Write the failing tests** (a `matchMedia` stub whose `MediaQueryList` records `change` listeners and can fire them; fake timers; `afterEach` calls the cleanup):
  - System mode, stub light → `setLightSkin("paper"); setDarkSkin("nord")`; flip the stub to dark and fire `change` → `data-skin="nord"`.
  - Sun mode at Berlin, `setSystemTime(2026-06-21T19:30:00Z)` (just before 19:33 sunset) → light slot. `vi.advanceTimersByTime(5 * SUN_RECHECK_MS)` → dark slot.
  - The interval only runs in sun mode: in manual mode `vi.getTimerCount() === 0`; switching to sun adds one, and switching back removes it.
  - Sleep: in sun mode, `vi.setSystemTime` jumps past sunset with no timers advanced, then `window.dispatchEvent(new Event("focus"))` → dark slot. Repeat with `document.dispatchEvent(new Event("visibilitychange"))` and `window.dispatchEvent(new Event("pageshow"))`.
  - Cross-tab: write `knot.skinMode = "system"` to storage, then dispatch `new StorageEvent("storage", { key: "knot.skinMode", newValue: "system" })` → `skinMode === "system"`. The same with `key: null` after `localStorage.clear()` → defaults (`"manual"`, `"light"`). An unrelated key (`"knot.docWidth"`) doesn't call `adoptStoredSkinPrefs` (spy).
  - With no `window.matchMedia`, starting the watcher doesn't throw.
  - The cleanup removes everything: after it, firing `change` / `focus` / `storage` changes nothing, and `vi.getTimerCount() === 0`.

- [ ] **Step 2: Run, expect FAIL:** `cd web && pnpm vitest run src/stores/skinWatcher.test.ts`

- [ ] **Step 3: Implement.** `refresh = () => useUi.getState().refreshSkin()`. Use `useUi.subscribe` to start or stop the interval when `skinMode` changes. `visibilitychange` only refreshes when `document.visibilityState === "visible"`. The storage handler reacts to `e.key === null` or any `SKIN_KEYS` value and calls `adoptStoredSkinPrefs()`. In `main.tsx`, call `watchSkinEnvironment()` after the stamps, with a one-line comment that it runs for the whole app lifetime, including the login, setup and share pages.

- [ ] **Step 4: Run, expect PASS,** and `cd web && pnpm test && pnpm tsc && pnpm lint`.

- [ ] **Step 5: Commit:** `feat(web): switch skins live on OS, sun and cross-tab changes`

---

### Task 6: Settings UI

**Files:**
- Modify: `web/src/features/workspace/SkinPicker.tsx`
- Create: `web/src/features/workspace/SunLocationPanel.tsx`
- Modify: `web/src/lib/sunLocation.ts` (add `parseCoordinate`, `formatLatLon`)
- Test: `web/src/features/workspace/SkinPicker.test.tsx` (extend; the existing four tests stay as they are), `web/src/features/workspace/SunLocationPanel.test.tsx`, `web/src/lib/sunLocation.test.ts` (extend)

**Interfaces:**
- Consumes: store API from Task 4; `sunDay` (Task 1); `resolveSunLocation`, `ResolvedSunLocation` (Task 2)
- Produces: `parseCoordinate(raw: string, limit: 90 | 180): number | null`; `formatLatLon(loc: LatLon): string` (`"52.52° N, 13.40° E"`, absolute values, 2 decimals, N/S and E/W)

UI copy, fixed by the spec:

| Element | testid | Text |
|---|---|---|
| mode radios (native `<input type="radio" name="skin-mode">` inside `<label>`, in a `<fieldset>` whose `sr-only` legend is "Skin switching", styled as a segmented control with existing tokens, visible focus ring) | `skin-mode-manual` / `skin-mode-system` / `skin-mode-sun` (on the inputs) | By hand · Follow system · Follow the sun |
| hint under the Light legend | `skin-hint-light` | system: "While your system is in light mode" · sun: "From sunrise to sunset" |
| hint under the Dark legend | `skin-hint-dark` | system: "While your system is in dark mode" · sun: "From sunset to sunrise" |
| marker on the card showing (automatic modes only) | `skin-now` | Now |
| times line | `sun-times` | "Light from {sunrise} to {sunset} today." · "The sun doesn't set today, so knot stays light." · "The sun doesn't rise today, so knot stays dark." |
| source line | `sun-location-source` | zone: "Location from your time zone, {zone}." · unplaced: "Your time zone ({zone or "unknown"}) has no known location, so knot assumes daylight from about 06:00 to 18:00. Set a location for real sunrise and sunset times." · typed: "Location set to {formatLatLon}." |
| buttons | `sun-location-edit`, `sun-location-reset` | "Set location" (not typed) / "Change" (typed); "Use time zone" (typed only) |
| form | `sun-location-form`, `sun-lat`, `sun-lon`, `sun-location-save`, `sun-location-cancel` | labels "Latitude", "Longitude"; buttons "Save", "Cancel" |
| error (`role="alert"`) | `sun-location-error` | "Latitude must be a number from -90 to 90." / "Longitude must be a number from -180 to 180." |

Times use `new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" })`. `sunDay` is called with local noon of today: `new Date(y, m, d, 12)`. The form starts prefilled with the location in use, at 2 decimals.

- [ ] **Step 1: Write the failing tests.**
  - `parseCoordinate`: `"52.52"` → 52.52, `" 52,52 "` → 52.52, `"+13.4"` → 13.4, `"-90"` → -90. `"1e2"`, `""`, `"abc"`, `"91"` (limit 90), `"-181"` (limit 180) and `"1.2.3"` → `null`.
  - `formatLatLon({ lat: -33.87, lon: -70.5 })` → `"33.87° S, 70.50° W"`.
  - SkinPicker: the radio for the current mode is checked. In sun mode, clicking `skin-paper` and `skin-nord` sets `lightSkin`/`darkSkin` and both cards end up `aria-pressed="true"` while `skin-light` is `"false"`. In manual mode, clicking a card calls the hand-pick path (`skinMode` stays `"manual"`). Hints render only in automatic modes, with the copy above. Exactly one `skin-now`, inside the card of the skin showing, and none in manual mode. Clicking `skin-mode-system` sets `skinMode`. `SunLocationPanel` renders only in sun mode.
  - SunLocationPanel (fake timers at `2026-06-21T10:00:00Z`; typed location Berlin via `setSunLocation`): `sun-times` contains the formatted sunrise and sunset of `sunDay(localNoon, berlin)` (compute the expectation with the same `Intl` formatter), `sun-location-source` reads "Location set to 52.52° N, 13.40° E." Tromsø in June → the polar-day sentence. Tromsø in December → the polar-night sentence. Edit → type `"91"` and save → the latitude error, store unchanged. Type `"48,14"` / `"11.58"` → store gets `{ lat: 48.14, lon: 11.58 }` and the form closes. "Use time zone" → `sunLocation` is `null` and the source line names the zone (spy `browserTimeZone`'s underlying `Intl` or stub via `vi.spyOn(Intl.DateTimeFormat.prototype, "resolvedOptions")` to return `Europe/Berlin`). Unplaced zone (`Etc/UTC`) → the unplaced sentence. Cancel closes without saving.

- [ ] **Step 2: Run, expect FAIL:** `cd web && pnpm vitest run src/features/workspace src/lib/sunLocation.test.ts`

- [ ] **Step 3: Implement.** Card `aria-pressed`: in manual mode `id === manualSkin`, otherwise `id === (scheme === "light" ? lightSkin : darkSkin)`. Click: in manual mode `setSkin`, otherwise the slot setter for the card's scheme. Hints connect to their fieldset with `aria-describedby`, and the legends stay exactly "Light" and "Dark".

- [ ] **Step 4: Run, expect PASS,** and `cd web && pnpm test && pnpm tsc && pnpm lint`.

- [ ] **Step 5: Commit:** `feat(web): settings to follow the system or the sun`

---

### Task 7: End-to-end flow and changelog

**Files:**
- Create: `e2e/flows/skin-auto.spec.ts`
- Modify: `CHANGELOG.md` (Unreleased → Added)

**Interfaces:**
- Consumes: the testids from Task 6; the storage keys from the Global Constraints

- [ ] **Step 1: Write `e2e/flows/skin-auto.spec.ts`** (`test.beforeAll(reset)`. Set up the owner as `e2e/flows/skin.spec.ts` does, with email `owner@skin-auto.test`):
  - **"follow system switches with the OS appearance":** `page.emulateMedia({ colorScheme: "light" })`, go to `/settings`, click `page.locator("label", { hasText: "Follow system" })`, click `skin-paper` and then `skin-nord`. Expect `<html data-skin="paper">` and `skin-now` inside `skin-paper`. Then `emulateMedia({ colorScheme: "dark" })` → `data-skin="nord"`, `data-theme="dark"` with no reload. A reload keeps `nord`. `emulateMedia` light → `paper`.
  - **`test.describe("follow the sun")` with `test.use({ timezoneId: "Europe/Berlin" })`:**
    - "light by day, dark after sunset, dark from first paint": `page.clock.install({ time: new Date("2026-06-21T10:00:00Z") })`, `goto("/login")`, set `knot.skinMode="sun"`, `knot.skinLight="paper"`, `knot.skinDark="nord"` via `page.evaluate`, reload → `paper`. Then `page.clock.setSystemTime(new Date("2026-06-21T19:40:00Z"))` and `page.clock.runFor(61_000)` → `nord`. Reload → `nord`.
    - "a typed location wins over the time zone": clock at `2026-12-21T11:00:00Z` (midday in Berlin), `knot.sunLocation='{"lat":69.65,"lon":18.96}'` (Tromsø, polar night) plus the three keys above → `nord`.

- [ ] **Step 2: Run** (needs the dev stack: Postgres on :5432, Dex on :5556, a :3001 → :3000 forwarder): `cd e2e && pnpm playwright test flows/skin-auto.spec.ts flows/skin.spec.ts`. Expected: all pass.

- [ ] **Step 3: Add the changelog entry** under `## [Unreleased]` → `### Added`, in the file's plain style: "Skins can switch on their own. In Settings → Appearance, choose Follow system to use your light skin while the operating system is in light mode and your dark skin while it is in dark mode, or Follow the sun to use the light skin from sunrise to sunset. knot works out where you are from your time zone, or from a location you type in. Nothing leaves the browser, and the choice is saved per browser like the skin itself."

- [ ] **Step 4: Commit:** `test(e2e): automatic skin switching` and `docs(changelog): automatic skin switching` (one commit is fine)

---

## Changes after the whole-branch review

The final review (four lenses, each finding independently verified) left no correctness defects. Its Minor findings were fixed in one pass, and a few places in this plan were superseded:

- **Unplaced zones (Task 2):** the longitude **wraps** to the UTC offset's exact meridian (`((-offsetMinutes / 4 + 540) % 360) - 180`) instead of clamping at ±180. For `Etc/GMT-14` that gives −150, not 180, so the "about 06:00 to 18:00" promise holds for UTC+13/+14 as well.
- **`America/Coral_Harbour` (Task 2):** the generator has a one-entry ICU alias override to `America/Atikokan`. Chrome reports ICU's canonical name for Atikokan, which tzdata's `backward` file links to Panama.
- **First paint (Task 4):** `main.tsx` stamps the skin the store already resolved (`useUi.getState().skin`). `resolveInitialSkin` is gone, and a fresh-module test covers the store's initial state instead.
- **Times (Task 6):** they are formatted with `{ timeStyle: "short" }`. The unplaced-zone sentence builds its 06:00 and 18:00 with the same formatter instead of hardcoding them.
- **Location form (Task 6):** every invalid field is flagged and the first one gets focus. The edit button is described by the times and source lines. U+2212 is accepted as a minus sign.
- **Forced colours (Task 6):** the segmented control marks the checked segment with `Highlight`, and keyboard focus is an outline, so both survive Windows High Contrast.
- **Tests (Task 7):** the follow-the-sun flow checks "paper" at 19:00Z, when only the time-zone table puts the sun up in Berlin, and checks `data-theme` after every reload.
