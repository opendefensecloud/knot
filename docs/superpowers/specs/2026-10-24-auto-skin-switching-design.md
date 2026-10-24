# Automatic light/dark skin switching: design

**Date:** 2026-10-24
**Status:** approved in conversation; builder calls marked *(builder call)*

## Why

knot has 13 skins (6 light, 7 dark) that a user picks by hand on the
Settings page. People who like a light skin by day and a dark one at night
switch by hand every evening and every morning. This feature does the
switching for them.

## What the user gets

The Appearance section on the Settings page offers three ways to choose a
skin:

1. **By hand.** Exactly today's behaviour: one skin, picked from the grid.
2. **Follow system.** knot shows the user's *light skin* while the operating
   system is in light mode and their *dark skin* while it is in dark mode.
   It switches the moment the OS does, without a reload.
3. **Follow the sun.** knot shows the light skin from sunrise to sunset
   and the dark skin the rest of the time. The times are the real ones for
   where the user is, so they move with the seasons.

Modes 2 and 3 each use two picks, one light skin and one dark skin, made
in the same card grid the page already has. The grid is already grouped
into Light and Dark, so in an automatic mode the user picks one card in
each group.

## Goals

- Switch between a chosen light skin and a chosen dark skin automatically,
  following either the OS appearance or real sunrise and sunset.
- Work fully offline. knot is often deployed on closed networks.
- Never show the wrong skin for a moment while the page loads.
- Change nothing for anyone who doesn't touch the new setting.

## Non-goals

- Storing the setting on the server or syncing it between devices. It
  lives in the browser like the skin, page width and sidebar width do
  today. (The user chose the simpler option.)
- Making Mermaid diagrams or Excalidraw boards follow the skin. They don't
  follow it today (Mermaid always uses its `default` theme, Excalidraw its
  own light theme), and this feature doesn't change that.
- Custom fixed hours ("dark from 19:00"), twilight offsets, or a dimming
  transition.
- A searchable city list or any geocoding.
- Changing what a brand-new visitor sees. They still get the Light skin,
  picked by hand.

## Anti-goals (failures even if it technically works)

- Any network request to an outside service, or a browser location
  permission prompt.
- A flash of the wrong skin on load. That includes a laptop opened after
  dark that was last used during the day.
- A user who picked a skin before this change seeing a different skin
  after it.

## Behaviour in detail

### Storage (per browser, `localStorage`)

| Key | Value | Missing / invalid means |
|---|---|---|
| `knot.skin` | the skin picked by hand (existing key, existing meaning, including the legacy `knot.theme = "dark"` fallback) | Light |
| `knot.skinMode` | `"manual"`, `"system"` or `"sun"` | `"manual"` |
| `knot.skinLight` | id of a skin whose scheme is light | `"light"` |
| `knot.skinDark` | id of a skin whose scheme is dark | `"dark"` |
| `knot.sunLocation` | JSON `{"lat": number, "lon": number}`, set only when the user typed a location | location from the time zone |

A slot holding a skin of the wrong scheme (e.g. `knot.skinLight = "nord"`)
counts as invalid. Every read and write survives a missing or throwing
`localStorage`, as the existing helpers in `web/src/stores/ui.ts` do.

### Which skin is showing

A pure function decides the skin from the preferences and the
environment:

- manual → the hand-picked skin
- system → `prefers-color-scheme: dark` matches ? dark slot : light slot.
  Without `matchMedia` (e.g. jsdom), treat the system as light.
- sun → daytime at the location ? light slot : dark slot

The store keeps `skin` as the skin **currently showing** and `theme` as
its scheme, so `data-skin`, `data-theme` and every existing consumer keep
working unchanged.

### Daytime

Daytime means the sun's centre is above −0.833° elevation (the standard
sunrise/sunset definition, which includes atmospheric refraction and the
sun's radius), computed with the NOAA solar-position equations in a small
pure module. Accuracy within a couple of minutes is plenty. Polar day
comes out as light all day and polar night as dark all day, with no
special casing.

For display, the same module also computes today's sunrise and sunset
(or reports polar day or polar night).

### Location

- **Default: from the time zone.** Read the browser's IANA time zone
  (`Intl.DateTimeFormat().resolvedOptions().timeZone`) and look it up in a
  table generated from tzdata's `zone.tab`, which lists coordinates for
  each zone's principal city. tzdata is public domain. The table also
  covers tzdata's `backward` link names, because Chrome reports
  ICU-canonical names such as `Asia/Calcutta` and `Europe/Kiev`. The table
  ships in the main bundle (it's needed before first paint) and must stay
  small: target under 10 KB gzipped. Commit the generator script next to
  the generated file. *(builder call: generator location and encoding)*
- **Unknown zone** (`UTC`, `Etc/GMT+5`, anything not in the table): use
  latitude 0 and the longitude implied by the current UTC offset
  (`−getTimezoneOffset() / 4` degrees). That gives a day of roughly
  06:00–18:00 local time. The settings say the time zone couldn't be
  placed and suggest typing a location. *(builder call)*
- **Typed location:** latitude and longitude in decimal degrees (−90…90,
  −180…180). Accept `.` or `,` as the decimal separator, round to 2
  decimals (about 1 km) when storing, and reject out-of-range or
  non-numeric input with an inline message. A "Use time zone" action
  clears it. *(builder call)*

### Live switching

A small controller started once from `web/src/main.tsx` (not a React
hook, so it also runs on the login, setup and public share pages) keeps
the showing skin current:

- a `matchMedia("(prefers-color-scheme: dark)")` change listener, which
  matters in system mode;
- in sun mode, a re-check every 60 seconds, plus on `visibilitychange`
  (becoming visible), `focus` and `pageshow`. This catches a laptop waking
  after sunset, a clock change, and a page restored from the
  back-forward cache. *(builder call: polling over scheduling the exact
  next sunrise/sunset, because timers don't survive sleep reliably and the
  computation costs microseconds)*;
- a `storage` listener for all five keys, so changing the setting in one
  tab updates the others. The listener only adopts the new values and
  doesn't write back. That's the same no-echo rule `AppShell` already
  follows for page width and sidebar width. *(builder call: today the
  skin doesn't sync across tabs; it does now)*

It only re-stamps `<html>` when the showing skin actually changes.

### Before first paint

`main.tsx` currently stamps `readInitialSkin()` before React renders. It
stamps the **resolved** skin instead, computed synchronously from
storage, `matchMedia`, the current time and the location. That's why the
time-zone table is in the main bundle.

### Settings UI

All of this lives in the existing Appearance section, built from what's
already there. *(builder call on exact copy and layout)*

- A three-way choice above the grid (a radio group styled as a segmented
  control): **By hand · Follow system · Follow the sun**.
- **By hand:** today's grid and behaviour. Exactly one card is pressed.
- **Automatic modes:** the same grid. Clicking a light card sets the light
  skin and clicking a dark card sets the dark skin, so one card per group
  is pressed. Under each group heading, a short line says when that skin
  is used ("While your system is in light mode" / "From sunrise to
  sunset", and the dark equivalents). The card currently showing carries
  a small "Now" marker. The group legends stay "Light" and "Dark".
- **Follow the sun only:** a line under the grid with today's times, e.g.
  "Light from 07:42 to 18:03 today" (or "The sun doesn't set today" / "The
  sun doesn't rise today"), and where the location comes from: "Location
  from your time zone, Europe/Berlin", or the typed coordinates. A "Set
  location" control opens latitude and longitude inputs with Save and
  Cancel. With a typed location set, the control offers "Use time zone"
  to go back. Times are formatted in the browser's locale.

Switching between modes:

- **Into an automatic mode** for the first time (no slot key stored yet):
  seed the slot matching the current skin's scheme with the current skin.
  A Nord user who turns on "Follow the sun" gets Nord at night and Light
  by day, not Dark. *(builder call)*
- **Into By hand:** the hand-picked skin becomes whatever is showing right
  now, so nothing visibly jumps. *(builder call)*
- `setSkin(id)` keeps its meaning, "pick this skin by hand": it sets the
  hand-picked skin and the mode to manual. The picker uses separate slot
  setters in automatic modes.

## Constraints

- Frontend only (`web/`). No server, API or migration change.
- Match the existing patterns: zustand store in `web/src/stores/ui.ts`,
  skin registry in `web/src/styles/skins.ts`, tokens in `tokens.css`,
  Tailwind classes using the existing tokens, vitest and testing-library
  tests next to the code, Playwright flows in `e2e/flows/`.
- No new runtime dependency.
- The existing `e2e/flows/skin.spec.ts` and unit tests keep passing
  unchanged in meaning.

## Testing

- **Unit:** solar math against reference sunrise and sunset times
  (±2 min) for a mid-latitude city at both solstices and an equinox, plus
  polar day and polar night (e.g. Tromsø in June and December); time-zone
  lookup including legacy link names and the unknown-zone fallback; the
  resolver for every mode; store persistence, defaults, invalid values,
  slot seeding and switching to By hand; cross-tab adoption; picker
  behaviour in each mode, including the location form's validation.
- **e2e:** Playwright's `emulateMedia({ colorScheme })` to show system
  mode switching live; `timezoneId` and `page.clock` to show sun mode
  choosing light by day and dark by night, including after a reload (the
  first-paint path); the setting surviving a reload.

## Docs

- `CHANGELOG.md`: an entry under Unreleased → Added.
- This spec and the implementation plan under `docs/superpowers/`.

## Calls settled after the builder check

- **Seeding** only fills slot keys that are missing. If the user already
  has a stored light skin but no dark skin, only the dark slot can be
  seeded, and only when the current skin is dark.
- **"Today's" sunrise and sunset** means the solar day around local noon
  of the browser's current calendar date at the location in use, shown
  in the browser's local clock time. For a typed location far from the
  browser's time zone, the times shown can fall on the previous or next
  day. That's acceptable: the switch itself always uses the elevation
  test at the current instant.
- **The time zone is re-read on every re-check**, which costs nothing, so
  a laptop that travels picks up its new zone.
- **The tz table generator** reads an extracted tzdata release directory
  (`zone.tab` + `backward`) given on the command line and writes the
  version (e.g. `2026e`) into the generated file's header. Link chains are
  resolved transitively. Links whose target has no coordinates (e.g.
  `Etc/*`) are skipped.
- **A unit test guards the size budget:** the generated table module must
  gzip to under 10 KB.

## Left to the builder

Module boundaries and file names beyond those named here, the time-zone
table's encoding, exact copy, and the segmented control's styling, as
long as it uses the existing tokens and is keyboard-accessible.
