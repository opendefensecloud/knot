import { useEffect, useState } from "react";
import { Navigate, useNavigate, useParams } from "react-router-dom";

import { isShortCode } from "../../lib/shortCode";
import { shortLinksApi } from "../../lib/shortLinks.api";

/** `/<code>`: open the page the short link names, or say it can't. */
export function ShortLinkResolver() {
  const { code = "" } = useParams();
  const nav = useNavigate();
  const [failure, setFailure] = useState<"missing" | "error" | null>(null);
  const valid = isShortCode(code);

  useEffect(() => {
    if (!valid) return;
    let live = true;
    void shortLinksApi.resolve(code).then((r) => {
      if (!live) return;
      if ("ok" in r) void nav(`/doc/${r.ok.doc_id}`, { replace: true });
      // Only a 404 means the link is dead; anything else may pass.
      else setFailure(r.error.status === 404 ? "missing" : "error");
    });
    return () => {
      live = false;
    };
  }, [code, valid, nav]);

  // Any other top-level path keeps today's behaviour: back to the start.
  if (!valid) return <Navigate to="/" replace />;
  if (failure === "error") {
    return (
      <div className="doc-shell text-fg-muted" data-testid="short-link-error">
        Couldn&apos;t open this link. Try again in a moment.
      </div>
    );
  }
  if (failure === "missing") {
    return (
      <div className="doc-shell text-fg-muted" data-testid="short-link-missing">
        This link doesn&apos;t exist or you don&apos;t have access to it.
      </div>
    );
  }
  return <div className="doc-shell text-fg-muted">Opening…</div>;
}
