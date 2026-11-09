-- doc_short_links
--
-- One short code per page (`https://<host>/<code>`). A code only names the
-- page; opening it still checks the reader's access. Codes never change and
-- are never reused; purging the page removes its code.
--
-- The FK takes SHARE ROW EXCLUSIVE on documents. Take it up front in
-- bounded attempts (knot_lock_tables, 20261107120000), so a long
-- transaction on documents makes this migration give up instead of
-- queueing every page write behind it.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('doc_short_links',
                        ARRAY['documents']::regclass[],
                        'SHARE ROW EXCLUSIVE');

CREATE TABLE doc_short_links (
    code        text PRIMARY KEY
                CHECK (code ~ '^[2-9a-kmnp-zA-HJ-NP-Z]{7}$' AND code ~ '[2-9]'),
    doc_id      uuid NOT NULL UNIQUE REFERENCES documents(id) ON DELETE CASCADE,
    created_by  uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
