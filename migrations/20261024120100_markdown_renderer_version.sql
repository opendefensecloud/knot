-- markdown_renderer_version
-- Created 2026-10-24
--
-- Which Markdown renderer wrote each cached row, so that a release whose
-- renderer writes better Markdown can refresh, once and in the background,
-- the rows an earlier one wrote (and the task rows indexed from them).
--
-- Every existing row gets 1 and is therefore stale. Only the current writer
-- (`put_rendered`) stamps a newer version: it sets `knot.renderer_version`
-- in its transaction, and the trigger copies it into the row. Every other
-- writer gets 1, including an old pod during a rolling upgrade, so its
-- render is swept again:
--   - with the setting, the row gets the setting's version;
--   - without it, an UPDATE gets 1;
--   - without it, an INSERT keeps the version it names (1 if none), so raw
--     SQL test seeds can write current rows.
ALTER TABLE doc_markdown_cache
  ADD COLUMN renderer_version int NOT NULL DEFAULT 1;

CREATE OR REPLACE FUNCTION doc_markdown_cache_renderer_version() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  v text := NULLIF(current_setting('knot.renderer_version', true), '');
BEGIN
  IF v IS NOT NULL THEN
    NEW.renderer_version := v::int;
  ELSIF TG_OP = 'UPDATE' THEN
    NEW.renderer_version := 1;
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER doc_markdown_cache_renderer_version
  BEFORE INSERT OR UPDATE ON doc_markdown_cache
  FOR EACH ROW EXECUTE FUNCTION doc_markdown_cache_renderer_version();
