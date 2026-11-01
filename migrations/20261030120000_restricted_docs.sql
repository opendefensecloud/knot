-- restricted_docs
-- Created 2026-10-30
--
-- A restricted document is a hard access boundary: inside its subtree the
-- workspace role stops applying and only explicit grants on the boundary or
-- below it give access. The partial index answers "does this workspace have
-- any restricted doc", and the grants index serves the per-principal loads
-- of the batched access fold. The DELETE drops `user:` grants of people who
-- are not members of the doc's workspace (see below).
ALTER TABLE documents ADD COLUMN restricted boolean NOT NULL DEFAULT false;
CREATE INDEX documents_restricted_idx ON documents (workspace_id) WHERE restricted;
CREATE INDEX document_grants_principal_idx ON document_grants (principal, doc_id);

-- Grants never outlive the membership they belong to. Before this release a
-- member's removal left their `user:` grants behind, and the grants route
-- accepted people outside the workspace. Such grants give nothing today, but
-- a re-invite or an OIDC re-provision would revive them, now possibly on a
-- restricted page. Drop them; `group:` grants are not tied to a member.
DELETE FROM document_grants g
 USING documents d
 WHERE g.doc_id = d.id
   AND g.principal LIKE 'user:%'
   AND NOT EXISTS (
       SELECT 1 FROM workspace_members m
        WHERE m.workspace_id = d.workspace_id
          AND g.principal = 'user:' || m.user_id::text
   );
