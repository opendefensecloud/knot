/**
 * Avatar — circular initial badge on a user-derived color.
 *
 * Matches the visual language of the WorkspaceHeader avatar and the
 * presence bar. Color is derived deterministically from the seed (usually
 * the user_id) so the same user always shows the same hue.
 */

type Size = "sm" | "md";

const sizes: Record<Size, string> = {
  sm: "h-5 w-5 text-[10px]",
  md: "h-6 w-6 text-[11px]",
};

export function Avatar({
  name,
  seed,
  size = "sm",
  title,
}: {
  name: string;
  seed: string;
  size?: Size;
  title?: string;
}) {
  return (
    <span
      aria-hidden
      title={title ?? name}
      className={`inline-flex items-center justify-center rounded-full text-white font-semibold select-none shrink-0 ${sizes[size]}`}
      style={{ background: colorFor(seed) }}
    >
      {(name || "?").slice(0, 1).toUpperCase()}
    </span>
  );
}

/**
 * A stable per-user colour.
 *
 * Emitted as 6-digit hex rather than `hsl()` because this same value is
 * published to collaborators through Yjs awareness, and y-prosemirror accepts
 * only `#rrggbb` for a peer caret — anything else logs "A user uses an
 * unsupported color format" once per peer, and the successor extension drops
 * the caret to `transparent` outright.
 *
 * The palette is unchanged: these are exactly the colours `hsl(h, 70%, 45%)`
 * produced, converted rather than re-picked, so nobody's avatar changes.
 */
export function colorFor(id: string): string {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  return hslToHex(hash % 360, 0.7, 0.45);
}

function hslToHex(h: number, s: number, l: number): string {
  const chroma = (1 - Math.abs(2 * l - 1)) * s;
  const secondary = chroma * (1 - Math.abs(((h / 60) % 2) - 1));
  const base = l - chroma / 2;
  const sector = Math.floor(h / 60) % 6;
  let r = 0;
  let g = 0;
  let b = 0;
  if (sector === 0) { r = chroma; g = secondary; }
  else if (sector === 1) { r = secondary; g = chroma; }
  else if (sector === 2) { g = chroma; b = secondary; }
  else if (sector === 3) { g = secondary; b = chroma; }
  else if (sector === 4) { r = secondary; b = chroma; }
  else { r = chroma; b = secondary; }
  const channel = (v: number) =>
    Math.round((v + base) * 255)
      .toString(16)
      .padStart(2, "0");
  return `#${channel(r)}${channel(g)}${channel(b)}`;
}
