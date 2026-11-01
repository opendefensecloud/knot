import { apiFetch } from "../../lib/api";
import { ReleaseNotes, WhatsNew, parse } from "../../lib/validators";

export const aboutApi = {
  /** The running build and the releases this user hasn't seen. */
  async whatsNew() {
    const r = await apiFetch<unknown>("/api/whats-new");
    if ("error" in r) return r;
    return { ok: parse(WhatsNew, r.ok) };
  },
  /** Record the running version as seen. The server never moves it back. */
  markSeen() {
    return apiFetch<void>("/api/whats-new/seen", { method: "POST" });
  },
  async releaseNotes() {
    const r = await apiFetch<unknown>("/api/release-notes");
    if ("error" in r) return r;
    return { ok: parse(ReleaseNotes, r.ok) };
  },
};
