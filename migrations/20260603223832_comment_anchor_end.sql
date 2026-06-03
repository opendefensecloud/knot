-- comment_anchor_end
-- Created 2026-06-03
--
-- Adds a second Yjs RelativePosition (the END of the anchored range) to
-- comments so we can paint a CRDT-stable highlight in the editor instead
-- of relying on anchor_text.length, which drifts after edits.
ALTER TABLE comments
  ADD COLUMN position_y_end BYTEA NULL;
