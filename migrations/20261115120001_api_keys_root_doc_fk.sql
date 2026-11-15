-- Subtree-scoped API keys, step 2 of 2 (S10).
--
-- Purging a key's root page deletes the key: ON DELETE CASCADE. Never
-- SET NULL, which would turn a subtree key into one for the whole
-- workspace. The constraint needs SHARE ROW EXCLUSIVE on both tables: it
-- blocks writes to documents and api_keys until COMMIT, no reads. Its
-- validation scans api_keys, where every root_doc_id is still NULL.
-- documents first: it is the busier table.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261115120001_api_keys_root_doc_fk',
                        ARRAY['documents', 'api_keys']::regclass[],
                        'SHARE ROW EXCLUSIVE');

ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_root_doc_id_fkey
    FOREIGN KEY (root_doc_id) REFERENCES documents(id) ON DELETE CASCADE;
