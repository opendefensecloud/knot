-- history_builder
-- Created 2026-11-08

-- Page history M1.3: the tables the history builder (crates/knot-history)
-- writes. It replays each page's rev-ordered log (M1.2) outside every room
-- and turns it into timeline entries. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md
-- (storage_model -> NEW TABLES 4-6, entry_builder) and the M1.3 plan's
-- Global Constraints and Rulings.
--   doc_history_state        one row per page the builder has visited: its
--                            watermark (built_rev, genesis_rev,
--                            snapshot_checked_rev, builder_version), the
--                            claims of content still parked as of built_rev,
--                            the rows (skip_revs), baselines
--                            (skip_baselines) and snapshot revs
--                            (skip_snapshots, whose probe failed) every
--                            replay or gap check leaves out, the
--                            failure counts and backoff (retry_after =
--                            'infinity' means given up), and the builder's
--                            lease (claimed_by, claimed_until). Every claim
--                            commits a crash marker (last_error = 'pass
--                            interrupted') before it computes, so a page that
--                            kills the process is backed off and then given
--                            up instead of being handed to every replica in
--                            turn.
--   doc_history_checkpoints  derived replay states: the genesis state, a
--                            periodic one whenever the row bytes since the
--                            last reach max(64 KiB, state size), and gap
--                            states (content a room snapshot held that no row
--                            carries, which only these rows keep once the
--                            snapshot is thinned). pending_revs lists the
--                            rows whose content was still parked at the
--                            checkpoint: the state leaves pending content
--                            out (yrs 0.27.3 encodes it at shifted ids), and
--                            every replay from the checkpoint re-applies
--                            those rows first.
--   doc_history_entries      the timeline: one entry per editing session,
--                            restore, copy, import, template, erase, baseline
--                            and gap, with ids only (author_id, baseline_id,
--                            the IdSets of what it inserted and deleted);
--                            names are resolved at read time, so renaming or
--                            anonymizing a person needs no rewrite here.
-- Checkpoints and entries are written only by the holder of the page's
-- builder lease, checked under the state row's FOR UPDATE, after the page
-- row's KEY SHARE (lock order: documents -> doc_history_state ->
-- doc_history_heads -> ...). Every table cascades with its page, and entries
-- also with their baseline.
--
-- sqlx runs this file in one transaction, as the Helm pre-upgrade Job, while
-- 0.9.10 pods keep serving and writing. The new tables' foreign keys need
-- SHARE ROW EXCLUSIVE on each table they reference: documents, users and
-- doc_history_baselines. Waiting for those locks in plain CREATE TABLE
-- statements would queue every page write, login and erase behind the wait,
-- and could deadlock with an app transaction that holds one of them and asks
-- for another. So knot_lock_tables (20261107120000) takes all three first,
-- documents (the busy one) first: it waits at most 100 ms per attempt for
-- documents, asks for the others with NOWAIT, and never waits for one table
-- while holding another. Once it holds them, nothing below waits for a lock
-- the app holds: the statements after it only create the new, empty tables
-- and their indexes. While the locks are held, page writes (creates,
-- renames, moves, archives, purges), users writes and baseline writes wait
-- until COMMIT; edit saves (the page row's KEY SHARE), snapshot writes and
-- page loads go on. Nothing takes ACCESS EXCLUSIVE.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261108120000_history_builder',
                        ARRAY['documents', 'users', 'doc_history_baselines']::regclass[],
                        'SHARE ROW EXCLUSIVE');

CREATE TABLE doc_history_state (
  doc_id               uuid PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  built_rev            bigint NOT NULL DEFAULT 0,
  genesis_rev          bigint NULL,
  builder_version      int NOT NULL DEFAULT 0,
  snapshot_checked_rev bigint NOT NULL DEFAULT 0,
  parked               jsonb NOT NULL DEFAULT '[]',
  skip_revs            bigint[] NOT NULL DEFAULT '{}',
  skip_baselines       bigint[] NOT NULL DEFAULT '{}',
  skip_snapshots       bigint[] NOT NULL DEFAULT '{}',
  failures             int NOT NULL DEFAULT 0,
  timeouts             int NOT NULL DEFAULT 0,
  crashes              int NOT NULL DEFAULT 0,
  retry_after          timestamptz NULL,
  last_error           text NULL,
  claimed_by           uuid NULL,
  claimed_until        timestamptz NULL,
  updated_at           timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE doc_history_checkpoints (
  doc_id       uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  rev          bigint NOT NULL,
  kind         text NOT NULL CHECK (kind IN ('genesis', 'periodic', 'gap')),
  state_bytes  bytea NOT NULL,
  state_vector bytea NOT NULL,
  pending_revs bigint[] NOT NULL DEFAULT '{}',
  created_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (doc_id, rev)
);

CREATE TABLE doc_history_entries (
  id            bigserial PRIMARY KEY,
  doc_id        uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  kind          text NOT NULL CHECK (kind IN ('session', 'restore', 'copy', 'import',
                                              'template', 'erase', 'baseline', 'gap')),
  author_id     uuid NULL REFERENCES users(id),
  started_at    timestamptz NOT NULL,
  ended_at      timestamptz NOT NULL,
  start_rev     bigint NULL,
  end_rev       bigint NULL,
  sealed        bool NOT NULL,
  baseline_id   bigint NULL REFERENCES doc_history_baselines(id) ON DELETE CASCADE,
  ins_ids       bytea NULL,
  del_ids       bytea NULL,
  words_added   int NULL,
  words_removed int NULL,
  words_pending bool NOT NULL DEFAULT false,
  meta          jsonb NOT NULL DEFAULT '{}',
  updated_at    timestamptz NOT NULL DEFAULT now(),
  CHECK (kind = 'baseline' OR end_rev IS NOT NULL)
);

-- The list, newest first, with a keyset cursor.
CREATE INDEX doc_history_entries_log_idx ON doc_history_entries (doc_id, end_rev DESC, id DESC);
-- A page's open sessions, by author.
CREATE INDEX doc_history_entries_open_idx ON doc_history_entries (doc_id, author_id) WHERE NOT sealed;
-- Open sessions idle long enough to seal.
CREATE INDEX doc_history_entries_idle_idx ON doc_history_entries (ended_at) WHERE NOT sealed;
-- Sessions whose word counts are still to be computed.
CREATE INDEX doc_history_entries_words_idx ON doc_history_entries (doc_id) WHERE words_pending;
-- The baseline FK's ON DELETE CASCADE probes it once per purged baseline;
-- without it each probe scans the whole table.
CREATE INDEX doc_history_entries_baseline_idx ON doc_history_entries (baseline_id)
  WHERE baseline_id IS NOT NULL;
