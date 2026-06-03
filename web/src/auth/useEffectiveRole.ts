import { useQuery } from "@tanstack/react-query";

import { docsApi } from "../features/docs/docs.api";

import { useSession } from "./SessionContext";

export type Role = "owner" | "editor" | "viewer";

/**
 * Two-dimensional role: workspace-level (from session) + per-doc
 * effective role (from /api/docs/:id). Pass docId to get both; pass
 * undefined to get only the workspace role.
 */
export function useEffectiveRole(docId?: string): { workspace: Role | null; doc: Role | null } {
  const session = useSession();
  const wsRole = session.data && "ok" in session.data ? session.data.ok.role : null;

  const docQ = useQuery({
    queryKey: ["doc", docId],
    queryFn: () => docsApi.get(docId!),
    enabled: Boolean(docId),
  });
  const docRole = docQ.data && "ok" in docQ.data ? docQ.data.ok.effective_role : null;

  return { workspace: wsRole, doc: docRole };
}
