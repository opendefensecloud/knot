export type Fallbacks = { diagrams: number; images: number };

/** Must match `file_name` in crates/knot-server/src/export.rs. */
const MAX_STEM_CODE_POINTS = 120;

function trimDotsAndSpaces(s: string): string {
  return s.replace(/^[ .]+|[ .]+$/g, "");
}

export function exportFileName(title: string, ext: "docx" | "odt"): string {
  const replaced = Array.from(title, (c) => {
    if (/[/\\:*?"<>|]/.test(c)) return "-";
    // Unicode Cc (C0, DEL, C1), as Rust's char::is_control.
    if (/\p{Cc}/u.test(c)) return "-";
    return c;
  }).join("");
  const capped = Array.from(trimDotsAndSpaces(replaced)).slice(0, MAX_STEM_CODE_POINTS).join("");
  const stem = trimDotsAndSpaces(capped) || "Untitled";
  return `${stem}.${ext}`;
}

/** Reads `X-Knot-Export-Fallbacks: diagrams=<n>, images=<m>, unknown=<k>`. */
export function parseFallbacks(header: string | null): Fallbacks {
  const out: Fallbacks = { diagrams: 0, images: 0 };
  if (!header) return out;
  for (const part of header.split(",")) {
    const m = /^\s*(diagrams|images)=(\d+)\s*$/.exec(part);
    if (m) out[m[1] as keyof Fallbacks] = Number(m[2]);
  }
  return out;
}

function count(n: number, singular: string): string {
  return `${n} ${singular}${n === 1 ? "" : "s"}`;
}

export function fallbackMessage(f: Fallbacks, unsynced: boolean): string | null {
  const parts: string[] = [];
  if (f.diagrams > 0) parts.push(count(f.diagrams, "diagram"));
  if (f.images > 0) parts.push(count(f.images, "image"));
  const messages: string[] = [];
  if (parts.length > 0) messages.push(`${parts.join(" and ")} couldn't be included.`);
  if (unsynced) messages.push("Your latest edits weren't saved yet and may be missing.");
  if (messages.length === 0) return null;
  return `Exported. ${messages.join(" ")}`;
}

export function saveBlob(blob: Blob, fileName: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  a.style.display = "none";
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Revoke after the click has been handled by the browser.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
