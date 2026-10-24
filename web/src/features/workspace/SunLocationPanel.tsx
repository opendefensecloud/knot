import { useEffect, useId, useRef, useState, type FormEvent } from "react";

import { sunDay, type LatLon, type SunDay } from "../../lib/sun";
import { formatLatLon, parseCoordinate, resolveSunLocation, type SunLocationSource } from "../../lib/sunLocation";
import { useUi } from "../../stores/ui";

type Field = keyof LatLon;
type Draft = Record<Field, string>;

const FIELDS: readonly Field[] = ["lat", "lon"];
const NONE: readonly Field[] = [];
const LABELS: Record<Field, string> = { lat: "Latitude", lon: "Longitude" };
const ERRORS: Record<Field, string> = {
  lat: "Latitude must be a number from -90 to 90.",
  lon: "Longitude must be a number from -180 to 180.",
};

/** Clock time in the browser's locale, on a 12- or 24-hour clock as it
 *  prefers. */
const clockTime = (at: Date) => new Intl.DateTimeFormat(undefined, { timeStyle: "short" }).format(at);

function timesLine(day: SunDay): string {
  switch (day.kind) {
    case "normal":
      return `Light from ${clockTime(day.sunrise)} to ${clockTime(day.sunset)} today.`;
    case "polar-day":
      return "The sun doesn't set today, so knot stays light.";
    case "polar-night":
      return "The sun doesn't rise today, so knot stays dark.";
  }
}

function sourceLine(source: SunLocationSource, loc: LatLon, now: Date): string {
  switch (source.kind) {
    case "typed":
      return `Location set to ${formatLatLon(loc)}.`;
    case "zone":
      return `Location from your time zone, ${source.zone}.`;
    case "unplaced": {
      // On the same clock as the times line above.
      const hour = (h: number) => clockTime(new Date(now.getFullYear(), now.getMonth(), now.getDate(), h));
      return (
        `Your time zone (${source.zone ?? "unknown"}) has no known location, so knot assumes daylight ` +
        `from about ${hour(6)} to ${hour(18)}. Set a location for real sunrise and sunset times.`
      );
    }
  }
}

const inputClass =
  "h-9 px-3 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent text-sm";
const primaryButtonClass =
  "h-9 px-3 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity";
const secondaryButtonClass =
  "h-9 px-3 rounded border border-border bg-surface text-fg text-sm font-medium hover:bg-muted transition-colors";

/**
 * "Follow the sun"'s details under the skin grid: today's sunrise and
 * sunset, where the sun is taken to be, and a form to type a location in
 * place of the time zone's.
 */
export function SunLocationPanel() {
  const typed = useUi((s) => s.sunLocation);
  const setSunLocation = useUi((s) => s.setSunLocation);
  // The form's text while it is open, null while it is closed.
  const [draft, setDraft] = useState<Draft | null>(null);
  // The fields the last Save turned down. A new array on every failed Save,
  // so each one moves focus again.
  const [invalid, setInvalid] = useState<readonly Field[]>(NONE);
  const id = useId();
  const timesId = `${id}-times`;
  const sourceId = `${id}-source`;
  const errorId = (field: Field) => `${id}-${field}-error`;
  const editButton = useRef<HTMLButtonElement>(null);
  const inputs = useRef<Partial<Record<Field, HTMLInputElement | null>>>({});

  // Today is the browser's calendar day, and its sun the solar day around
  // local noon, wherever the location is.
  const now = new Date();
  const { loc, source } = resolveSunLocation(typed, now);
  const day = sunDay(new Date(now.getFullYear(), now.getMonth(), now.getDate(), 12), loc);

  // Closing the form hands focus back to the button that opened it.
  const editing = draft !== null;
  const wasEditing = useRef(false);
  useEffect(() => {
    if (!editing && wasEditing.current) editButton.current?.focus();
    wasEditing.current = editing;
  }, [editing]);

  // A failed Save moves focus to the first field it turned down, after the
  // render that gives the field its message, so a screen reader reads the
  // message out, even when the same Save fails twice.
  useEffect(() => {
    const first = invalid[0];
    if (first) inputs.current[first]?.focus();
  }, [invalid]);

  function close() {
    setDraft(null);
    setInvalid(NONE);
  }

  function save(e: FormEvent) {
    e.preventDefault();
    if (!draft) return;
    const lat = parseCoordinate(draft.lat, 90);
    const lon = parseCoordinate(draft.lon, 180);
    if (lat === null || lon === null) {
      // Every field that is wrong says so, not just the first.
      const parsed = { lat, lon };
      setInvalid(FIELDS.filter((field) => parsed[field] === null));
      return;
    }
    setSunLocation({ lat, lon });
    close();
  }

  return (
    <div>
      <p id={timesId} data-testid="sun-times" className="text-sm text-fg m-0">
        {timesLine(day)}
      </p>
      <p id={sourceId} data-testid="sun-location-source" className="text-[13px] text-fg-muted m-0 mt-0.5">
        {sourceLine(source, loc, now)}
      </p>
      {draft ? (
        <form data-testid="sun-location-form" onSubmit={save} className="grid gap-3 max-w-sm mt-3">
          <div className="grid grid-cols-2 gap-3">
            {FIELDS.map((field) => (
              <label key={field} className="grid gap-1 text-[13px] text-fg-muted">
                {LABELS[field]}
                {/* Plain text, not inputMode="decimal": iOS's decimal pad has no minus sign. */}
                <input
                  ref={(el) => {
                    inputs.current[field] = el;
                  }}
                  data-testid={`sun-${field}`}
                  type="text"
                  autoComplete="off"
                  spellCheck={false}
                  autoFocus={field === "lat"}
                  value={draft[field]}
                  onChange={(e) => {
                    const { value } = e.target;
                    setDraft((d) => d && { ...d, [field]: value });
                  }}
                  aria-invalid={invalid.includes(field)}
                  aria-describedby={invalid.includes(field) ? errorId(field) : undefined}
                  className={inputClass}
                />
              </label>
            ))}
          </div>
          {invalid.length > 0 && (
            <div data-testid="sun-location-error" role="alert" className="grid gap-1 text-destructive text-[13px]">
              {invalid.map((field) => (
                <p key={field} id={errorId(field)} className="m-0">
                  {ERRORS[field]}
                </p>
              ))}
            </div>
          )}
          <div className="flex gap-2">
            <button data-testid="sun-location-save" type="submit" className={primaryButtonClass}>
              Save
            </button>
            <button data-testid="sun-location-cancel" type="button" onClick={close} className={secondaryButtonClass}>
              Cancel
            </button>
          </div>
        </form>
      ) : (
        <div className="flex flex-wrap gap-2 mt-3">
          <button
            ref={editButton}
            data-testid="sun-location-edit"
            type="button"
            // Focus lands here after Save, Cancel and "Use time zone", so
            // it reads out the times and the location they leave in place.
            aria-describedby={`${timesId} ${sourceId}`}
            onClick={() => setDraft({ lat: loc.lat.toFixed(2), lon: loc.lon.toFixed(2) })}
            className={secondaryButtonClass}
          >
            {source.kind === "typed" ? "Change" : "Set location"}
          </button>
          {source.kind === "typed" && (
            <button
              data-testid="sun-location-reset"
              type="button"
              onClick={() => {
                setSunLocation(null);
                // This button goes away; the one beside it stays.
                editButton.current?.focus();
              }}
              className={secondaryButtonClass}
            >
              Use time zone
            </button>
          )}
        </div>
      )}
    </div>
  );
}
