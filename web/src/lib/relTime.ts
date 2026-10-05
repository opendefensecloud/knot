/**
 * Compact "how long ago" label: "just now", "5m ago", "3h ago", "2d ago".
 *
 * Whole units, rounded down, no months or years — the callers (comment
 * timestamps, the contributor list) show the absolute time on hover, so this
 * only has to be glanceable. A timestamp slightly in the future (server and
 * client clocks disagree) reads as "just now" rather than a negative age.
 */
export function relTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}
