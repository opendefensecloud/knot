/**
 * LibraryReturn — the page `libraries.excalidraw.com` redirects to after a
 * user clicks "Add to Excalidraw". We're set as the `libraryReturnUrl` on
 * our Excalidraw mount. The hash carries `#addLibrary=<.excalidrawlib url>`
 * and optionally a token.
 *
 * Happy path: `window.opener` is the original tab that opened the library
 * picker. We postMessage the library URL back to it (the modal listens) and
 * close ourselves. The library install happens inside the still-mounted
 * Excalidraw without any further navigation.
 *
 * Fallback: opener is null (some browsers null it across cross-origin
 * redirects). Stash the URL in sessionStorage so the modal can drain it on
 * its next mount, then send the user back to where they came from.
 */

import { useEffect, useState } from "react";

const PENDING_LIBRARY_KEY = "knot.pendingLibrary";

type Opener = { postMessage: (data: unknown, targetOrigin: string) => void } | null;

function getOpener(): Opener {
  // window.opener is `any` in lib.dom; narrow it through this helper.
  const o = (window as unknown as { opener?: unknown }).opener;
  if (!o || typeof o !== "object") return null;
  return o as Opener;
}

export default function LibraryReturn() {
  const [message, setMessage] = useState("Importing library…");

  useEffect(() => {
    const hash = window.location.hash.replace(/^#/, "");
    const params = new URLSearchParams(hash);
    const libraryUrl = params.get("addLibrary");
    const token = params.get("token") ?? null;

    if (!libraryUrl) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setMessage("No library URL in the redirect. You can close this tab.");
      return;
    }

    const payload = { type: "knot:add-library" as const, libraryUrl, token };
    const opener = getOpener();
    if (opener) {
      try {
        opener.postMessage(payload, window.location.origin);
        setMessage("Library added. You can close this tab.");
        // Some browsers refuse to close non-script-opened windows; fall
        // through to the visible message in that case.
        window.setTimeout(() => {
          try { window.close(); } catch { /* noop */ }
        }, 100);
        return;
      } catch (err) {
        console.warn("library postMessage failed, falling back", err);
      }
    }

    // No opener — stash and bounce. The modal drains sessionStorage on mount.
    try {
      window.sessionStorage.setItem(
        PENDING_LIBRARY_KEY,
        JSON.stringify({ libraryUrl, token }),
      );
    } catch (err) {
      console.warn("library stash failed", err);
    }
    setMessage("Library queued. Reopen the diagram to finish importing.");
    if (window.history.length > 1) window.history.back();
  }, []);

  return (
    <div className="min-h-screen grid place-items-center px-6 text-center text-fg-muted">
      <p>{message}</p>
    </div>
  );
}

export { PENDING_LIBRARY_KEY };
