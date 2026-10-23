/**
 * Wording for the About page and the upgrade card. The card names the new
 * version and how many releases the user missed; it never summarises them —
 * that is what the release notes on the About page are for.
 */

/** The upgrade card's count of releases since the user's last visit. */
export function missedReleasesText(count: number): string {
  return count === 1
    ? "1 new release since your last visit"
    : `${count} releases since your last visit`;
}

/** A commit hash as people quote it. */
export function shortCommit(commit: string): string {
  return commit.slice(0, 7);
}

/**
 * A changelog date ("2026-10-21") for display. It is a calendar date, not an
 * instant: formatted in UTC, where it parses, so no time zone moves it to
 * the day before.
 */
export function formatReleaseDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
    timeZone: "UTC",
  });
}
