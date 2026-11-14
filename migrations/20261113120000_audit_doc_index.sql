-- audit_doc_index (page audit log)
--
-- The page audit log (GET /api/docs/{id}/audit) reads one page's events
-- newest first and counts them by action and exporter. audit_events had
-- only its primary key, so every read would scan the whole table.
-- (target_id, id) serves `WHERE target_id = $doc ORDER BY id DESC` as a
-- backward index scan; workspace_id, target_kind and action stay residual
-- filters (uuids don't repeat across kinds).
--
-- CREATE INDEX takes SHARE on audit_events, which blocks every audit
-- INSERT while it builds: the ones inside the transactions that create,
-- rename, move, archive, restore, duplicate and purge pages (the archive
-- retention job's purge too), mark and unmark templates, restrict pages
-- and change their grants, which wait with them; and the best-effort
-- writes of member invites, role changes and removals and of quota
-- refusals. Edit saves never touch the table. Take the lock in bounded
-- attempts (knot_lock_tables, 20261107120000) so a long transaction makes
-- this migration give up instead of queueing page writes behind it.
-- Before this release the table grew only with those actions; from now on
-- every export adds one row per exported page as well.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('audit_doc_index',
                        ARRAY['audit_events']::regclass[],
                        'SHARE');

CREATE INDEX IF NOT EXISTS audit_events_target_idx
    ON audit_events (target_id, id);
