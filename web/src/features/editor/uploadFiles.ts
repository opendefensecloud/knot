import type { ApiError, ApiResult } from "../../lib/api";
import type { BlobResponse } from "../../lib/blobs.api";
import {
  LIMIT_TOAST_KEY,
  isRateLimited,
  limitErrorText,
  retryAfterSecs,
  type Notify,
} from "../../lib/limitErrors";

/** The longest a dropped file waits for the upload rate limit before its
 *  one retry. */
export const MAX_RETRY_WAIT_MS = 30_000;

/** The wait when a 429 names none (the server always does). */
const DEFAULT_RETRY_WAIT_MS = 5_000;

/** A wait longer than this is announced, so the drop doesn't look stuck. */
const PAUSE_NOTICE_AFTER_MS = 2_000;

export type UploadDeps = {
  upload: (file: File) => Promise<ApiResult<BlobResponse>>;
  insert: (blob: BlobResponse, file: File) => void;
  notify: Notify;
  /** Should resolve early once `signal` aborts (see `abortableWait`). */
  wait: (ms: number) => Promise<void>;
  /** Aborted when the drop's doc or editor goes away: nothing more is
   *  uploaded or inserted after that. */
  signal: AbortSignal;
};

/** Waits `ms`, or less if `signal` aborts first. */
export function abortableWait(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise<void>((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const finish = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", finish);
      resolve();
    };
    const timer = setTimeout(finish, ms);
    signal.addEventListener("abort", finish);
  });
}

/** Why one file of a drop failed, for anything but a limit. */
export function uploadErrorText(error: ApiError): string {
  switch (error.code) {
    case "blob.too_large": return "File too large (10 MB cap).";
    case "blob.blocked_type": return "File type not allowed.";
    case "acl.no_grant": return "You don't have permission to upload here.";
    default: return "Upload failed.";
  }
}

function filesNotUploaded(count: number): string {
  return count === 1 ? "1 file wasn't uploaded." : `${count} files weren't uploaded.`;
}

function retryWaitMs(error: ApiError): number {
  const seconds = retryAfterSecs(error);
  return seconds === null ? DEFAULT_RETRY_WAIT_MS : Math.min(seconds * 1000, MAX_RETRY_WAIT_MS);
}

/**
 * Uploads a paste, drop or picked set of files one at a time, inserting
 * each as it is stored.
 *
 * A file refused by the upload rate limit waits what the server asks (at
 * most 30 s; a wait over 2 s is announced under `LIMIT_TOAST_KEY`, shown
 * for the whole wait) and is tried once more. A limit that still refuses
 * it, or a quota, stops the drop with one toast, replacing that notice,
 * saying how many files weren't uploaded. Any other failure is reported
 * for that file and the rest go on.
 *
 * Once `deps.signal` aborts, nothing more is uploaded or inserted; a pause
 * notice still showing is replaced to say the drop stopped.
 */
export async function uploadFiles(files: File[], deps: UploadDeps): Promise<void> {
  for (const [index, file] of files.entries()) {
    if (deps.signal.aborted) return;
    let result = await deps.upload(file);
    if ("error" in result && isRateLimited(result.error)) {
      const waitMs = retryWaitMs(result.error);
      const pauseNoticeShown = waitMs > PAUSE_NOTICE_AFTER_MS;
      if (pauseNoticeShown) {
        deps.notify("info", `Uploads paused — continuing in ${Math.ceil(waitMs / 1000)} s…`, {
          key: LIMIT_TOAST_KEY,
          durationMs: waitMs,
        });
      }
      await deps.wait(waitMs);
      if (deps.signal.aborted) {
        if (pauseNoticeShown) {
          deps.notify("info", `Uploads stopped. ${filesNotUploaded(files.length - index)}`, { key: LIMIT_TOAST_KEY });
        }
        return;
      }
      result = await deps.upload(file);
    }
    if (deps.signal.aborted) return;
    if ("error" in result) {
      const limitText = limitErrorText(result.error);
      if (limitText !== null) {
        deps.notify("warn", `${limitText} ${filesNotUploaded(files.length - index)}`, { key: LIMIT_TOAST_KEY });
        return;
      }
      deps.notify("error", uploadErrorText(result.error));
      continue;
    }
    deps.insert(result.ok, file);
  }
}
