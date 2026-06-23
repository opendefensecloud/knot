-- heal_doc_cycles
-- Promote any document that is its own ancestor (a parent cycle) to the
-- workspace root by nulling its parent_id. Depth-capped so a pre-existing
-- cycle cannot loop forever. Idempotent; a no-op on healthy data.
WITH RECURSIVE anc(start, cur, depth) AS (
    SELECT id, parent_id, 1
    FROM documents
    WHERE parent_id IS NOT NULL
    UNION ALL
    SELECT a.start, d.parent_id, a.depth + 1
    FROM anc a
    JOIN documents d ON d.id = a.cur
    WHERE a.cur IS NOT NULL AND a.depth < 1000
)
UPDATE documents
SET parent_id = NULL, updated_at = now()
WHERE id IN (SELECT start FROM anc WHERE cur = start);
