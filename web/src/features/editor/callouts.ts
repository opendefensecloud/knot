/**
 * Callout boxes: a blockquote with a `kind` (one of GitHub's and GitLab's
 * five alert kinds) and an optional plain-text `title`. The Rust side is
 * `crates/knot-markdown/src/callout.rs`.
 */

import { Info, Lightbulb, MessageSquareWarning, OctagonAlert, TriangleAlert, type LucideIcon } from "lucide-react";

export const CALLOUT_KINDS = ["note", "tip", "important", "warning", "caution"] as const;
export type CalloutKind = (typeof CALLOUT_KINDS)[number];

/** The name shown when a box has no title of its own. */
export const CALLOUT_LABEL: Record<CalloutKind, string> = {
  note: "Note",
  tip: "Tip",
  important: "Important",
  warning: "Warning",
  caution: "Caution",
};

export const CALLOUT_ICON: Record<CalloutKind, LucideIcon> = {
  note: Info,
  tip: Lightbulb,
  important: MessageSquareWarning,
  warning: TriangleAlert,
  caution: OctagonAlert,
};

/** `raw` as a kind, in any case; null for anything else. */
export function calloutKind(raw: unknown): CalloutKind | null {
  if (typeof raw !== "string") return null;
  const k = raw.trim().toLowerCase();
  return (CALLOUT_KINDS as readonly string[]).includes(k) ? (k as CalloutKind) : null;
}

/** `raw` as a title: one line, trimmed ("" when nothing is left). */
export function normaliseTitle(raw: string): string {
  return raw.split(/\s+/).filter(Boolean).join(" ");
}
