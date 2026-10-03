import { Check } from "lucide-react";

import { useUi } from "../../stores/ui";
import { SKINS, type Scheme, type Skin } from "../../styles/skins";

/**
 * Card grid for choosing a skin, grouped light / dark.
 *
 * Each card carries its own `data-skin`, so the tokens in tokens.css scope
 * to it and the card previews itself — background, surface, text, body
 * font, accent and corner radius — with no colours repeated in TS.
 */
export function SkinPicker() {
  return (
    <div className="space-y-4">
      <SkinGroup scheme="light" label="Light" />
      <SkinGroup scheme="dark" label="Dark" />
    </div>
  );
}

function SkinGroup({ scheme, label }: { scheme: Scheme; label: string }) {
  const skins = SKINS.filter((skin) => skin.scheme === scheme);
  return (
    <fieldset className="m-0 p-0 border-0 min-w-0">
      <legend className="text-sm text-fg mb-2 p-0">{label}</legend>
      <div className="grid grid-cols-2 sm:grid-cols-3 gap-3">
        {skins.map((skin) => <SkinCard key={skin.id} skin={skin} />)}
      </div>
    </fieldset>
  );
}

function SkinCard({ skin }: { skin: Skin }) {
  const active = useUi((s) => s.skin === skin.id);
  const setSkin = useUi((s) => s.setSkin);
  return (
    <button
      type="button"
      data-testid={`skin-${skin.id}`}
      data-skin={skin.id}
      aria-pressed={active}
      onClick={() => setSkin(skin.id)}
      className={`text-left rounded-lg border bg-bg p-2 transition-[box-shadow,border-color] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
        active ? "border-accent ring-2 ring-accent" : "border-border hover:border-fg-muted"
      }`}
    >
      <div className="relative rounded border border-border bg-surface px-3 py-2.5 min-h-[92px]">
        <span
          aria-hidden
          className="absolute top-2.5 right-2.5 flex h-4 w-4 items-center justify-center rounded-full bg-accent text-accent-fg"
        >
          {active && <Check size={11} strokeWidth={3} />}
        </span>
        <p className="m-0 pr-6 font-body text-[15px] font-semibold leading-tight text-fg">{skin.name}</p>
        <p className="m-0 mt-1 text-[12px] leading-snug text-fg-muted">{skin.blurb}</p>
      </div>
    </button>
  );
}
