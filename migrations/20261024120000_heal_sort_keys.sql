-- heal_sort_keys
-- Rewrite the order keys of every sibling group holding a key that the new
-- generator can't place a page next to, then make keys unique at the top
-- level too. A no-op on healthy data, and safe to run again.
--
-- A group is the rows of one parent in a workspace, archived ones included:
-- they keep their key and count towards uniqueness. A group is rewritten
-- when one of its keys is invalid (not `^[0-9a-z]*[1-9a-z]$`: empty, outside
-- the alphabet, or ending in '0'), over 64 characters, or repeated. The old
-- UNIQUE (workspace_id, parent_id, sort_key) treated NULL parents as
-- distinct, so top-level keys could repeat.
--
-- A workspace's root list is rewritten as one list: its top-level rows and
-- the children of its templates, which the sidebar shows at the top level,
-- sorted together with them. When one of its groups needs rewriting, all of
-- them are, so the order users see is kept, and each group stays unique
-- because every key of the combined list is distinct.
--
-- Rows keep the order users see: the sidebar sorts by code unit, so by
-- `sort_key COLLATE "C"`, then `parent_id NULLS FIRST`, then `created_at`,
-- then `id`. The parent only decides between groups of a root list that
-- share a key: the sidebar lists top-level pages first, then template
-- children by parent (`list_alive`), and its stable sort by key keeps that.
-- Within a group the parent is the same, so it changes nothing there.
--
-- A rewritten list of n rows gets `lexorank::spread(n)`
-- (crates/knot-storage/src/lexorank.rs), which pg_temp.knot_spread_key
-- computes. Only `sort_key` changes: to users the page is the same, so
-- `updated_at` stays.
--
-- crates/knot-server/tests/markdown_compat.rs (`SORT_KEY_GROUPS`) reports
-- the same groups, read-only, on a database before the upgrade. Keep the two
-- in step.
--
-- The first statement locks `documents` until the migration commits, so a
-- pod still on the old release waits, and can't write a key between the
-- heal and the new constraint.

ALTER TABLE documents
    DROP CONSTRAINT IF EXISTS documents_workspace_id_parent_id_sort_key_key;

-- Key `i` (from 1) of `n` evenly spaced keys, as `lexorank::spread(n)`: with
-- `w` the smallest width where 36^w > 2(n+1), floor(i * 36^w / (n+1)) as `w`
-- base-36 digits, padded with '0' on the left, without its trailing '0's.
CREATE OR REPLACE FUNCTION pg_temp.knot_spread_key(i bigint, n bigint) RETURNS text
LANGUAGE plpgsql IMMUTABLE STRICT AS $$
DECLARE
    width int := 1;
    space bigint := 36;
    v bigint;
    digits text := '';
BEGIN
    WHILE space <= 2 * (n + 1) LOOP
        width := width + 1;
        space := space * 36;
    END LOOP;
    v := i * space / (n + 1);
    FOR d IN 1..width LOOP
        digits := substr('0123456789abcdefghijklmnopqrstuvwxyz', (v % 36)::int + 1, 1)
                  || digits;
        v := v / 36;
    END LOOP;
    RETURN rtrim(digits, '0');
END;
$$;

-- One statement for every list to rewrite: the old constraint is gone, and
-- the new one is added once all keys are distinct.
WITH keyed AS (
    -- Each row, whether its key is bad, and the list it is rewritten in:
    -- `list_parent` is NULL for its workspace's root list, else its parent.
    SELECT d.id,
           d.workspace_id,
           d.parent_id,
           CASE WHEN d.parent_id IS NULL OR coalesce(p.is_template, false) THEN NULL
                ELSE d.parent_id
           END AS list_parent,
           d.sort_key,
           d.created_at,
           d.sort_key COLLATE "C" !~ '^[0-9a-z]*[1-9a-z]$'
               OR length(d.sort_key) > 64
               OR count(*) OVER (PARTITION BY d.workspace_id, d.parent_id,
                                              d.sort_key COLLATE "C") > 1 AS bad
      FROM documents d
      LEFT JOIN documents p ON p.id = d.parent_id
),
lists AS (
    SELECT keyed.*,
           bool_or(bad) OVER (PARTITION BY workspace_id, list_parent) AS rewrite
      FROM keyed
),
respread AS (
    -- Every row of a list to rewrite, with its key by its rank in the
    -- sidebar's order (see the header).
    SELECT id,
           pg_temp.knot_spread_key(
               row_number() OVER (PARTITION BY workspace_id, list_parent
                                  ORDER BY sort_key COLLATE "C", parent_id NULLS FIRST,
                                           created_at, id),
               count(*) OVER (PARTITION BY workspace_id, list_parent)
           ) AS sort_key
      FROM lists
     WHERE rewrite
)
UPDATE documents d
   SET sort_key = r.sort_key
  FROM respread r
 WHERE d.id = r.id
   AND d.sort_key IS DISTINCT FROM r.sort_key;

-- Unique per parent, the top level included. Checked at the end of each
-- statement, so one UPDATE can rewrite a whole group's keys.
ALTER TABLE documents
    DROP CONSTRAINT IF EXISTS documents_sort_key_unique;
ALTER TABLE documents
    ADD CONSTRAINT documents_sort_key_unique
    UNIQUE NULLS NOT DISTINCT (workspace_id, parent_id, sort_key)
    DEFERRABLE INITIALLY IMMEDIATE;

DROP FUNCTION pg_temp.knot_spread_key(bigint, bigint);
