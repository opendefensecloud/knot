-- Subtree-scoped API keys, step 1 of 2 (S10,
-- specs/2026-11-13-subtree-scoped-api-keys-design.md).
--
-- root_doc_id NULL keeps today's meaning: the key reaches what its user
-- reaches. Set, the key reaches only that page and its descendants. The
-- foreign key (ON DELETE CASCADE, never SET NULL, which would silently
-- widen the key to the whole workspace) follows in step 2 under a lock
-- that doesn't block page loads.
--
-- A nullable column without a default is a catalog change only, but needs
-- ACCESS EXCLUSIVE on api_keys, which every key-authenticated request
-- reads. Take it in bounded attempts (knot_lock_tables, 20261107120000).
-- The previous release never names the column, so it keeps working.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261114120000_api_keys_root_doc',
                        ARRAY['api_keys']::regclass[],
                        'ACCESS EXCLUSIVE');

ALTER TABLE api_keys ADD COLUMN root_doc_id uuid NULL;
CREATE INDEX api_keys_root_doc_idx ON api_keys (root_doc_id)
    WHERE root_doc_id IS NOT NULL;

-- S10: the root lives in the key's workspace, and a key never changes its
-- root (no widening, no re-targeting). documents has no
-- UNIQUE (workspace_id, id), so a composite foreign key isn't available.
CREATE OR REPLACE FUNCTION api_keys_root_doc_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'UPDATE' THEN
    IF NEW.root_doc_id IS DISTINCT FROM OLD.root_doc_id THEN
      RAISE EXCEPTION 'api key % cannot change its root page', NEW.id
        USING ERRCODE = 'KB005';
    END IF;
    RETURN NEW;
  END IF;
  IF NEW.root_doc_id IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM documents
                      WHERE id = NEW.root_doc_id
                        AND workspace_id = NEW.workspace_id) THEN
    RAISE EXCEPTION 'api key root page % is not in workspace %',
      NEW.root_doc_id, NEW.workspace_id
      USING ERRCODE = 'KB005';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER api_keys_root_doc_guard
    BEFORE INSERT OR UPDATE OF root_doc_id ON api_keys
    FOR EACH ROW EXECUTE FUNCTION api_keys_root_doc_guard();
