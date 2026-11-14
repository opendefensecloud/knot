/**
 * What the app says when the server refuses a user's action for a rate
 * limit (429 `rate.limited`) or a quota (403 `quota.exceeded`), issue #88.
 * The server's answers are fixed by
 * docs/superpowers/specs/2026-11-12-rate-limits-design.md and built by
 * `rate_limited` (`details.retry_after_secs`) in
 * crates/knot-server/src/http_error.rs and `refusal_response`
 * (`details {quota, scope, limit}`, quota `pages`/`blob_bytes`, scope
 * `user`/`workspace`) in crates/knot-server/src/quota.rs.
 */

import type { NotifyOptions, Toast } from "../stores/ui";

/** The part of an `ApiError` these helpers read. */
export type LimitErrorInput = {
  status: number;
  code: string;
  details?: Record<string, unknown>;
};

/** The dedupe key every limit toast shares: a second one replaces the first. */
export const LIMIT_TOAST_KEY = "limit";

const RATE_LIMITED_CODE = "rate.limited";
const QUOTA_EXCEEDED_CODE = "quota.exceeded";

const QUOTA_TEXTS: Record<string, Record<string, string>> = {
  pages: {
    user: "You've reached your page limit.",
    workspace: "This workspace has reached its page limit.",
  },
  blob_bytes: {
    user: "You've reached your file storage limit.",
    workspace: "This workspace has reached its file storage limit.",
  },
};

/** A quota the server named in a way this bundle doesn't know yet. */
const UNKNOWN_QUOTA_TEXT = "You've reached a storage limit.";

export function isRateLimited(error: LimitErrorInput): boolean {
  return error.code === RATE_LIMITED_CODE;
}

export function isQuotaExceeded(error: LimitErrorInput): boolean {
  return error.code === QUOTA_EXCEEDED_CODE;
}

/** The whole seconds a rate limit asks to wait (`details.retry_after_secs`),
 *  or null when this isn't a rate limit or it names no usable wait. */
export function retryAfterSecs(error: LimitErrorInput): number | null {
  if (!isRateLimited(error)) return null;
  const seconds = error.details?.retry_after_secs;
  if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds <= 0) return null;
  return Math.ceil(seconds);
}

/** The text for a rate-limit or quota refusal, or null for any other error. */
export function limitErrorText(error: LimitErrorInput): string | null {
  if (isRateLimited(error)) {
    const seconds = retryAfterSecs(error);
    return seconds === null
      ? "You're doing that too fast. Try again in a moment."
      : `You're doing that too fast. Try again in ${seconds} s.`;
  }
  if (isQuotaExceeded(error)) {
    const quota = error.details?.quota;
    const scope = error.details?.scope;
    const text = typeof quota === "string" && typeof scope === "string" ? QUOTA_TEXTS[quota]?.[scope] : undefined;
    return text ?? UNKNOWN_QUOTA_TEXT;
  }
  return null;
}

export type Notify = (kind: Toast["kind"], text: string, opts?: NotifyOptions) => void;

/**
 * Says why a user's action failed: a limit's own text as a warning (one at
 * a time, under `LIMIT_TOAST_KEY`), else `fallbackText` as an error. With
 * no `fallbackText`, other failures stay silent, as they were.
 */
export function notifyFailure(
  notify: Notify,
  error: LimitErrorInput | null | undefined,
  fallbackText: string | null,
): void {
  const limitText = error ? limitErrorText(error) : null;
  if (limitText !== null) {
    notify("warn", limitText, { key: LIMIT_TOAST_KEY });
    return;
  }
  if (fallbackText !== null) notify("error", fallbackText);
}
