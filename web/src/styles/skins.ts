/** The skin registry. The colours and typography for each entry live in
 *  `tokens.css` under `[data-skin="<id>"]`; this file is only what the
 *  picker and the store need to know: which skins exist, what to call
 *  them, and whether each one is a light or a dark scheme. */

export type Scheme = "light" | "dark";

export type Skin = {
  id: string;
  name: string;
  scheme: Scheme;
  /** One line for the picker card. */
  blurb: string;
};

export const SKINS: readonly Skin[] = [
  // ── Light ────────────────────────────────────────────────────────────
  { id: "light", name: "Light", scheme: "light", blurb: "Clean slate and blue. The default." },
  { id: "paper", name: "Paper", scheme: "light", blurb: "Warm cream, ink text, a serif for reading." },
  { id: "solarized-light", name: "Solarized Light", scheme: "light", blurb: "The classic cream, with readable body text." },
  { id: "rose-pine-dawn", name: "Rosé Pine Dawn", scheme: "light", blurb: "Rosy off-white with a pine accent." },
  { id: "sage", name: "Sage", scheme: "light", blurb: "Mint-white, deep green, softer corners." },
  { id: "high-contrast", name: "High Contrast", scheme: "light", blurb: "Pure white, black text, strong borders." },
  // ── Dark ─────────────────────────────────────────────────────────────
  { id: "dark", name: "Dark", scheme: "dark", blurb: "Deep slate and blue. The default dark." },
  { id: "nord", name: "Nord", scheme: "dark", blurb: "Polar-night greys with a frost accent." },
  { id: "gruvbox", name: "Gruvbox", scheme: "dark", blurb: "Warm charcoal, cream text, orange accent." },
  { id: "catppuccin-mocha", name: "Catppuccin Mocha", scheme: "dark", blurb: "Deep indigo, lavender text, mauve accent." },
  { id: "everforest", name: "Everforest", scheme: "dark", blurb: "Mossy green-grey with khaki text." },
  { id: "dracula", name: "Dracula", scheme: "dark", blurb: "The classic, purple accent." },
  { id: "terminal", name: "Terminal", scheme: "dark", blurb: "True black, phosphor green, monospace everywhere." },
];

export const DEFAULT_SKIN_ID = "light";

const byId = new Map(SKINS.map((skin) => [skin.id, skin]));

export function findSkin(id: string | null | undefined): Skin | undefined {
  return id == null ? undefined : byId.get(id);
}

/** Which of `light` / `dark` a skin id belongs to, for `data-theme` and
 *  `color-scheme`. Unknown ids resolve to the default skin's scheme. */
export function schemeOf(id: string): Scheme {
  return (findSkin(id) ?? findSkin(DEFAULT_SKIN_ID)!).scheme;
}
