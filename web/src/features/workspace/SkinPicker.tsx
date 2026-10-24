import { Check } from "lucide-react";
import { useId } from "react";

import { useUi } from "../../stores/ui";
import type { SkinMode } from "../../styles/skinPrefs";
import { SKINS, type Scheme, type Skin } from "../../styles/skins";

import { SunLocationPanel } from "./SunLocationPanel";

/**
 * How the skin is chosen, and the card grid to choose it from, grouped
 * light / dark. By hand the pressed card is the skin; in an automatic mode
 * each group holds one pick, and the card showing right now is marked.
 *
 * Each card carries its own `data-skin`, so the tokens in tokens.css scope
 * to it and the card previews itself — background, surface, text, body
 * font, accent and corner radius — with no colours repeated in TS.
 */
export function SkinPicker() {
  const mode = useUi((s) => s.skinMode);
  return (
    <div className="space-y-4">
      <SkinModeChoice mode={mode} />
      <SkinGroup mode={mode} scheme="light" label="Light" />
      <SkinGroup mode={mode} scheme="dark" label="Dark" />
      {mode === "sun" && <SunLocationPanel />}
    </div>
  );
}

const MODES: readonly { mode: SkinMode; label: string }[] = [
  { mode: "manual", label: "By hand" },
  { mode: "system", label: "Follow system" },
  { mode: "sun", label: "Follow the sun" },
];

/** When each group's pick shows, in each automatic mode. */
const HINTS: Record<Exclude<SkinMode, "manual">, Record<Scheme, string>> = {
  system: { light: "While your system is in light mode", dark: "While your system is in dark mode" },
  sun: { light: "From sunrise to sunset", dark: "From sunset to sunrise" },
};

/** Native radios, so the arrow keys move between the modes, styled as a
 *  segmented control. Full width on a phone, where the labels may wrap.
 *  Forced colours drop backgrounds and box shadows, so there the chosen
 *  segment takes the system highlight and the focus ring is an outline. */
function SkinModeChoice({ mode }: { mode: SkinMode }) {
  const setSkinMode = useUi((s) => s.setSkinMode);
  return (
    <fieldset className="m-0 p-0 border-0 min-w-0">
      <legend className="sr-only">Skin switching</legend>
      <div className="grid grid-cols-3 sm:inline-grid gap-1 rounded border border-border bg-bg p-1">
        {MODES.map((option) => (
          <label key={option.mode} className="relative flex">
            <input
              type="radio"
              name="skin-mode"
              value={option.mode}
              data-testid={`skin-mode-${option.mode}`}
              checked={mode === option.mode}
              onChange={() => setSkinMode(option.mode)}
              className="peer sr-only"
            />
            <span className="flex flex-1 items-center justify-center rounded-sm px-2 sm:px-3 py-1.5 text-center text-sm font-medium leading-tight text-fg-muted cursor-pointer transition-colors hover:bg-muted hover:text-fg peer-checked:bg-accent peer-checked:text-accent-fg peer-checked:forced-colors:bg-[Highlight] peer-checked:forced-colors:text-[HighlightText] peer-focus-visible:outline peer-focus-visible:outline-2 peer-focus-visible:outline-offset-1 peer-focus-visible:outline-accent">
              {option.label}
            </span>
          </label>
        ))}
      </div>
    </fieldset>
  );
}

function SkinGroup({ mode, scheme, label }: { mode: SkinMode; scheme: Scheme; label: string }) {
  const skins = SKINS.filter((skin) => skin.scheme === scheme);
  const hint = mode === "manual" ? null : HINTS[mode][scheme];
  const hintId = useId();
  return (
    <fieldset className="m-0 p-0 border-0 min-w-0" aria-describedby={hint ? hintId : undefined}>
      <legend className={`text-sm text-fg p-0 ${hint ? "mb-0.5" : "mb-2"}`}>{label}</legend>
      {hint && (
        <p id={hintId} data-testid={`skin-hint-${scheme}`} className="text-[13px] text-fg-muted m-0 mb-2">
          {hint}
        </p>
      )}
      <div className="grid grid-cols-2 sm:grid-cols-3 gap-3">
        {skins.map((skin) => <SkinCard key={skin.id} skin={skin} mode={mode} />)}
      </div>
    </fieldset>
  );
}

function SkinCard({ skin, mode }: { skin: Skin; mode: SkinMode }) {
  const manual = mode === "manual";
  const light = skin.scheme === "light";
  // By hand a card is pressed when it is the skin picked; in an automatic
  // mode, when it is its scheme's pick.
  const picked = useUi((s) => (manual ? s.manualSkin : light ? s.lightSkin : s.darkSkin) === skin.id);
  const pick = useUi((s) => (manual ? s.setSkin : light ? s.setLightSkin : s.setDarkSkin));
  // By hand the skin showing is the pressed card, so only the automatic
  // modes mark it.
  const showing = useUi((s) => !manual && s.skin === skin.id);
  return (
    <button
      type="button"
      data-testid={`skin-${skin.id}`}
      data-skin={skin.id}
      aria-pressed={picked}
      onClick={() => pick(skin.id)}
      className={`text-left rounded-lg border bg-bg p-2 transition-[box-shadow,border-color] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
        picked ? "border-accent ring-2 ring-accent" : "border-border hover:border-fg-muted"
      }`}
    >
      <div className="relative rounded border border-border bg-surface px-3 py-2.5 min-h-[92px]">
        <span
          aria-hidden
          className="absolute top-2.5 right-2.5 flex h-4 w-4 items-center justify-center rounded-full bg-accent text-accent-fg"
        >
          {picked && <Check size={11} strokeWidth={3} />}
        </span>
        <p className="m-0 pr-6 font-body text-[15px] font-semibold leading-tight text-fg">
          {skin.name}
          {showing && (
            <span
              data-testid="skin-now"
              className="ml-1.5 inline-block rounded-sm bg-accent px-1.5 align-middle font-sans text-[11px] font-medium leading-4 text-accent-fg"
            >
              Now
            </span>
          )}
        </p>
        <p className="m-0 mt-1 text-[12px] leading-snug text-fg-muted">{skin.blurb}</p>
      </div>
    </button>
  );
}
