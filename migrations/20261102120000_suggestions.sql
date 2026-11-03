-- suggestions
-- Created 2026-11-02

-- Change suggestions live beside the page, like comments: the proposed text
-- never enters the Y.Doc until an owner accepts. position_y / position_y_end
-- are Yjs RelativePositions (base64 on the wire, bytea here); they are equal
-- for an insertion. Lengths are in characters, matching the route's
-- `.chars().count()`.
--
-- status moves only through conditional UPDATEs: pending → applying (the
-- accept claim) → accepted, applying → pending (release), and
-- pending → rejected | withdrawn.
--
-- apply_token is minted by the first accept claim and kept from then on.
-- The room records it as the value of the doc's applied marker, a root
-- Y.Map any Editor can write, so only a marker holding it counts as
-- applied. Server-side only: never sent to clients.
CREATE TABLE suggestions (
  id              uuid PRIMARY KEY,
  doc_id          uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  author_id       uuid NOT NULL REFERENCES users(id),
  kind            text NOT NULL CHECK (kind IN ('replace', 'insert', 'delete')),
  position_y      bytea NOT NULL,
  position_y_end  bytea NOT NULL,
  original_text   text NOT NULL CHECK (length(original_text) <= 10000),
  context_before  text NOT NULL DEFAULT '' CHECK (length(context_before) <= 200),
  proposed_text   text NOT NULL CHECK (length(proposed_text) <= 10000),
  reason          text NULL CHECK (reason IS NULL OR length(reason) <= 4096),
  status          text NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'applying', 'accepted', 'rejected', 'withdrawn')),
  decided_by      uuid NULL REFERENCES users(id),
  decided_at      timestamptz NULL,
  apply_token     uuid NULL,
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);
-- The per-doc listing, oldest first.
CREATE INDEX suggestions_doc_idx ON suggestions(doc_id, created_at);
-- The sweep over claims that never finished.
CREATE INDEX suggestions_applying_idx ON suggestions(updated_at) WHERE status = 'applying';

-- Reply threads on suggestions, plain text like comment replies.
CREATE TABLE suggestion_replies (
  id             uuid PRIMARY KEY,
  suggestion_id  uuid NOT NULL REFERENCES suggestions(id) ON DELETE CASCADE,
  author_id      uuid NOT NULL REFERENCES users(id),
  body           text NOT NULL CHECK (length(body) > 0 AND length(body) <= 4096),
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now(),
  deleted_at     timestamptz NULL
);
-- Not partial: the ON DELETE CASCADE from suggestions looks replies up by
-- suggestion_id, deleted or not.
CREATE INDEX suggestion_replies_suggestion_idx
  ON suggestion_replies(suggestion_id, created_at);

-- Two new inbox kinds. The inline CHECK in 20261101120000_notifications.sql
-- got Postgres's default name, notifications_kind_check.
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check
  CHECK (kind IN ('mention', 'reply', 'task_assigned', 'task_due', 'doc_shared',
                  'suggestion', 'suggestion_decided'));
