import { useQuery } from "@tanstack/react-query";
import { useParams } from "react-router-dom";

import { apiErrorFrom } from "../../lib/api";
import { limitErrorText } from "../../lib/limitErrors";

type Fetched = { status: number; html: string };

/** The wait a 429 from the share link's per-IP limit asks for, as text.
 *  Any 429 here is that limit, envelope or not. */
function rateLimitedText(body: string): string {
  let parsed: unknown;
  try { parsed = JSON.parse(body); } catch { parsed = undefined; }
  const error = apiErrorFrom(429, parsed);
  return limitErrorText({ ...error, code: "rate.limited" }) ?? "";
}

export default function PublicDoc() {
  const { token } = useParams<{ token: string }>();
  const q = useQuery({
    queryKey: ["public", token],
    queryFn: async (): Promise<Fetched> => {
      // cache: "no-store" so a revoked token surfaces as 410 immediately
      // instead of replaying the previous 200 from the browser HTTP cache.
      const r = await fetch(`/p/${encodeURIComponent(token!)}`, {
        credentials: "omit",
        cache: "no-store",
      });
      return { status: r.status, html: await r.text() };
    },
    enabled: Boolean(token),
    retry: false,
    staleTime: 0,
  });

  if (q.isLoading) {
    return <main style={{ padding: 40, textAlign: "center" }}>Loading…</main>;
  }
  if (!q.data) {
    return <main style={{ padding: 40, textAlign: "center" }}>Failed to load.</main>;
  }
  if (q.data.status === 410) {
    return (
      <main style={{ padding: 40, textAlign: "center" }}>
        <h1>Link expired or revoked</h1>
        <p style={{ color: "#666" }}>This share link is no longer active.</p>
      </main>
    );
  }
  if (q.data.status === 429) {
    return (
      <main style={{ padding: 40, textAlign: "center" }}>
        <h1>Too many requests</h1>
        <p style={{ color: "#666" }}>{rateLimitedText(q.data.html)}</p>
      </main>
    );
  }
  if (q.data.status === 503) {
    return (
      <main style={{ padding: 40, textAlign: "center" }}>
        <h1>Still rendering</h1>
        <p style={{ color: "#666" }}>This document is still rendering. Try again shortly.</p>
      </main>
    );
  }
  if (q.data.status !== 200) {
    return <main style={{ padding: 40, textAlign: "center" }}>Not found.</main>;
  }
  return (
    <div
      data-testid="public-doc"
      // The server's HTML is a full document (skeleton + body). Embed via iframe
      // srcdoc so its <style> doesn't leak into the SPA's frame.
      style={{ position: "fixed", inset: 0 }}
    >
      <iframe
        title="Public document"
        srcDoc={q.data.html}
        style={{ width: "100%", height: "100%", border: "none" }}
        sandbox="allow-same-origin"
      />
    </div>
  );
}
