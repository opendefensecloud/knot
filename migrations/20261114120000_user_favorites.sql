-- user_favorites
--
-- Pages a user starred, shown in the sidebar's Favorites section, oldest
-- star first (`sort_key`, so drag-reorder can come later without a
-- migration). A star is a private preference: reads filter rows to pages
-- the user can still read, so a favorite whose page became hidden stays
-- stored and comes back when access does. Purging the page, deleting the
-- user or deleting the workspace removes its rows (ON DELETE CASCADE).
-- `workspace_id` is stored so reads filter by workspace without a join.
--
-- The three FKs take SHARE ROW EXCLUSIVE on documents, users and
-- workspaces. Take them up front in bounded attempts (knot_lock_tables,
-- 20261107120000), which never waits on one table while holding another,
-- so a long transaction on any of them makes this migration give up
-- instead of queueing every page write behind it.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('user_favorites',
                        ARRAY['documents', 'users', 'workspaces']::regclass[],
                        'SHARE ROW EXCLUSIVE');

CREATE TABLE user_favorites (
    user_id      uuid NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    doc_id       uuid NOT NULL REFERENCES documents(id)  ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sort_key     text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, doc_id)
);
CREATE INDEX user_favorites_user_ws_sort
    ON user_favorites (user_id, workspace_id, sort_key COLLATE "C");
