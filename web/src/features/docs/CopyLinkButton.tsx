import { Link as LinkIcon } from "lucide-react";
import { useState } from "react";

import type { ApiError } from "../../lib/api";
import { notifyFailure } from "../../lib/limitErrors";
import { shortLinksApi } from "../../lib/shortLinks.api";
import { useUi } from "../../stores/ui";

/**
 * Copies the page's short link, for every reader. A live page gets its code
 * on first use; an archived page only copies a code it already has, else
 * its long `/doc/<id>` link. When the clipboard refuses (insecure context,
 * denied permission) the link is shown to copy by hand. A refused link (a
 * rate limit, say) is said in a toast.
 */
export function CopyLinkButton({ docId, archived }: { docId: string; archived: boolean }) {
  const notify = useUi((s) => s.notify);
  const [fallback, setFallback] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  /** The link to copy, or why there is none. */
  async function linkToCopy(): Promise<{ link: string } | { error: ApiError }> {
    const r = archived ? await shortLinksApi.get(docId) : await shortLinksApi.getOrCreate(docId);
    if ("ok" in r) return { link: r.ok.url };
    if (archived && r.error.status === 404) return { link: `${window.location.origin}/doc/${docId}` };
    return { error: r.error };
  }

  /**
   * Start the clipboard write now, inside the click: WebKit refuses a write
   * that begins after an await (the link is fetched first), so where
   * `ClipboardItem` exists the write gets a promise of the text instead.
   */
  function copy(url: Promise<string | null>): Promise<void> {
    const clip = typeof navigator !== "undefined" ? navigator.clipboard : undefined;
    if (clip?.write && typeof ClipboardItem !== "undefined") {
      const blob = url.then((u) =>
        u ? new Blob([u], { type: "text/plain" }) : Promise.reject(new Error("no link")),
      );
      return clip.write([new ClipboardItem({ "text/plain": blob })]);
    }
    return url.then((u) => (u ? navigator.clipboard.writeText(u) : Promise.reject(new Error("no link"))));
  }

  async function onClick() {
    setBusy(true);
    const answer = linkToCopy();
    const url = answer.then((a) => ("link" in a ? a.link : null));
    const copied = copy(url);
    try {
      const outcome = await answer;
      if ("error" in outcome) {
        notifyFailure(notify, outcome.error, "Couldn't get a link for this page.");
        return;
      }
      const link = outcome.link;
      try {
        await copied;
        setFallback(null);
        notify("info", "Copied!");
      } catch {
        setFallback(link);
      }
    } finally {
      copied.catch(() => {});
      setBusy(false);
    }
  }

  return (
    <span className="relative inline-flex">
      <button
        type="button"
        data-testid="copy-link"
        aria-label="Copy link"
        title="Copy a short link to this page"
        disabled={busy}
        onClick={() => {
          void onClick();
        }}
        className="inline-flex items-center gap-1.5 h-9 px-3 rounded text-[13px] font-medium text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150 disabled:opacity-60"
      >
        <LinkIcon size={16} aria-hidden />
        <span>Copy link</span>
      </button>
      {fallback && (
        <span className="absolute top-full right-0 mt-1 z-20 w-72 bg-surface border border-border rounded shadow-sm p-2">
          <input
            data-testid="copy-link-fallback"
            aria-label="Link to copy"
            readOnly
            autoFocus
            value={fallback}
            onFocus={(e) => e.currentTarget.select()}
            onBlur={() => setFallback(null)}
            className="w-full h-8 px-2 rounded border border-border bg-bg text-[13px] text-fg"
          />
        </span>
      )}
    </span>
  );
}
