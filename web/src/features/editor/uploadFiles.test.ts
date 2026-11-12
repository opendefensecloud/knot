import { afterEach, describe, expect, it, vi } from "vitest";

import type { ApiError, ApiResult } from "../../lib/api";
import type { BlobResponse } from "../../lib/blobs.api";
import { LIMIT_TOAST_KEY } from "../../lib/limitErrors";
import { MAX_RETRY_WAIT_MS, abortableWait, uploadFiles, type UploadDeps } from "./uploadFiles";

function file(name: string): File {
  return new File(["x"], name, { type: "text/plain" });
}

function stored(name: string): ApiResult<BlobResponse> {
  return {
    ok: { id: name, doc_id: "d1", content_type: "text/plain", byte_size: 1, url: `/b/${name}`, original_name: name },
  };
}

function refused(error: Partial<ApiError> & { code: string; status: number }): ApiResult<BlobResponse> {
  return { error: { message: error.code, details: {}, ...error } };
}

const TOO_FAST = refused({ status: 429, code: "rate.limited", details: { retry_after_secs: 7 } });
const OVER_QUOTA = refused({ status: 403, code: "quota.exceeded", details: { quota: "blob_bytes", scope: "user", limit: 10 } });

/** Deps whose upload answers from `replies` in turn. */
function deps(replies: ApiResult<BlobResponse>[]) {
  const queue = [...replies];
  const uploaded: string[] = [];
  const inserted: string[] = [];
  const waits: number[] = [];
  const notify = vi.fn();
  const abortController = new AbortController();
  const uploadDeps: UploadDeps = {
    upload: (f) => {
      uploaded.push(f.name);
      const next = queue.shift();
      if (!next) throw new Error(`no reply left for ${f.name}`);
      return Promise.resolve(next);
    },
    insert: (blob) => { inserted.push(blob.id); },
    notify,
    wait: (ms) => { waits.push(ms); return Promise.resolve(); },
    signal: abortController.signal,
  };
  return { uploadDeps, uploaded, inserted, waits, notify, abortController };
}

describe("uploadFiles", () => {
  it("uploads and inserts every file in order", async () => {
    const d = deps([stored("a"), stored("b")]);
    await uploadFiles([file("a"), file("b")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a", "b"]);
    expect(d.inserted).toEqual(["a", "b"]);
    expect(d.notify).not.toHaveBeenCalled();
  });

  it("waits the asked seconds on a 429 and retries that file once", async () => {
    const d = deps([stored("a"), TOO_FAST, stored("b"), stored("c")]);
    await uploadFiles([file("a"), file("b"), file("c")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a", "b", "b", "c"]);
    expect(d.waits).toEqual([7000]);
    expect(d.inserted).toEqual(["a", "b", "c"]);
  });

  it("says the drop is paused, under the limit key, before a long wait", async () => {
    const d = deps([TOO_FAST, stored("a")]);
    d.uploadDeps.wait = (ms) => {
      expect(d.notify).toHaveBeenCalledExactlyOnceWith("info", "Uploads paused — continuing in 7 s…", {
        key: LIMIT_TOAST_KEY,
        durationMs: 7000,
      });
      d.waits.push(ms);
      return Promise.resolve();
    };
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.waits).toEqual([7000]);
    expect(d.notify).toHaveBeenCalledOnce();
  });

  it("names the capped wait in the paused notice", async () => {
    const longWait = refused({ status: 429, code: "rate.limited", details: { retry_after_secs: 600 } });
    const d = deps([longWait, stored("a")]);
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.notify).toHaveBeenCalledExactlyOnceWith("info", "Uploads paused — continuing in 30 s…", {
      key: LIMIT_TOAST_KEY,
      durationMs: MAX_RETRY_WAIT_MS,
    });
  });

  it("retries a short wait without a notice", async () => {
    const shortWait = refused({ status: 429, code: "rate.limited", details: { retry_after_secs: 2 } });
    const d = deps([shortWait, stored("a")]);
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.waits).toEqual([2000]);
    expect(d.notify).not.toHaveBeenCalled();
  });

  it("waits at most 30 seconds", async () => {
    const longWait = refused({ status: 429, code: "rate.limited", details: { retry_after_secs: 600 } });
    const d = deps([longWait, stored("a")]);
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.waits).toEqual([MAX_RETRY_WAIT_MS]);
    expect(MAX_RETRY_WAIT_MS).toBe(30_000);
  });

  it("stops with one toast, replacing the paused notice, when the retry is refused again", async () => {
    const d = deps([TOO_FAST, TOO_FAST]);
    await uploadFiles([file("a"), file("b"), file("c")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a", "a"]);
    expect(d.inserted).toEqual([]);
    expect(d.notify.mock.calls).toEqual([
      ["info", "Uploads paused — continuing in 7 s…", { key: LIMIT_TOAST_KEY, durationMs: 7000 }],
      ["warn", "You're doing that too fast. Try again in 7 s. 3 files weren't uploaded.", { key: LIMIT_TOAST_KEY }],
    ]);
  });

  it("stops at a quota with one toast saying how many files weren't uploaded", async () => {
    const d = deps([stored("a"), OVER_QUOTA]);
    await uploadFiles([file("a"), file("b"), file("c")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a", "b"]);
    expect(d.inserted).toEqual(["a"]);
    expect(d.waits).toEqual([]);
    expect(d.notify).toHaveBeenCalledExactlyOnceWith(
      "warn",
      "You've reached your file storage limit. 2 files weren't uploaded.",
      { key: LIMIT_TOAST_KEY },
    );
  });

  it("names a single file that wasn't uploaded", async () => {
    const d = deps([OVER_QUOTA]);
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.notify).toHaveBeenCalledExactlyOnceWith(
      "warn",
      "You've reached your file storage limit. 1 file wasn't uploaded.",
      { key: LIMIT_TOAST_KEY },
    );
  });

  it("stops at a quota met on the retry, too", async () => {
    const d = deps([TOO_FAST, OVER_QUOTA]);
    await uploadFiles([file("a"), file("b")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a", "a"]);
    expect(d.notify).toHaveBeenLastCalledWith(
      "warn",
      "You've reached your file storage limit. 2 files weren't uploaded.",
      { key: LIMIT_TOAST_KEY },
    );
  });

  it("says why other files failed and carries on, as before", async () => {
    const d = deps([refused({ status: 413, code: "blob.too_large" }), stored("b")]);
    await uploadFiles([file("a"), file("b")], d.uploadDeps);
    expect(d.notify).toHaveBeenCalledExactlyOnceWith("error", "File too large (10 MB cap).");
    expect(d.inserted).toEqual(["b"]);
  });

  // Leaving the page (or the editor going away) aborts the drop: the files
  // left must not land in the old doc's blobs or in whatever editor is
  // showing by then.
  it("stops before any further upload or insert once aborted during the wait", async () => {
    const d = deps([TOO_FAST]);
    d.uploadDeps.wait = (ms) => {
      d.waits.push(ms);
      d.abortController.abort();
      return Promise.resolve();
    };
    await uploadFiles([file("a"), file("b")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a"]);
    expect(d.inserted).toEqual([]);
    // The paused notice no longer holds; it is replaced, not left counting down.
    expect(d.notify.mock.calls).toEqual([
      ["info", "Uploads paused — continuing in 7 s…", { key: LIMIT_TOAST_KEY, durationMs: 7000 }],
      ["info", "Uploads stopped. 2 files weren't uploaded.", { key: LIMIT_TOAST_KEY }],
    ]);
  });

  it("does not insert a file stored after the abort, nor upload the rest", async () => {
    const d = deps([stored("a"), stored("b")]);
    const upload = d.uploadDeps.upload;
    d.uploadDeps.upload = (f) => {
      d.abortController.abort();
      return upload(f);
    };
    await uploadFiles([file("a"), file("b")], d.uploadDeps);
    expect(d.uploaded).toEqual(["a"]);
    expect(d.inserted).toEqual([]);
    expect(d.notify).not.toHaveBeenCalled();
  });

  it("uploads nothing once aborted", async () => {
    const d = deps([stored("a")]);
    d.abortController.abort();
    await uploadFiles([file("a")], d.uploadDeps);
    expect(d.uploaded).toEqual([]);
    expect(d.inserted).toEqual([]);
  });
});

describe("abortableWait", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("resolves after the wait", async () => {
    vi.useFakeTimers();
    const settled = vi.fn();
    void abortableWait(5000, new AbortController().signal).then(settled);
    await vi.advanceTimersByTimeAsync(4999);
    expect(settled).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(settled).toHaveBeenCalledOnce();
  });

  it("resolves at once on abort and clears its timer", async () => {
    vi.useFakeTimers();
    const abortController = new AbortController();
    const settled = vi.fn();
    void abortableWait(30_000, abortController.signal).then(settled);
    abortController.abort();
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("resolves at once when already aborted", async () => {
    const abortController = new AbortController();
    abortController.abort();
    await expect(abortableWait(30_000, abortController.signal)).resolves.toBeUndefined();
  });
});
