-- api_tokens
-- Created 2026-11-01, renumbered 2026-11-02
--
-- Was 20261101120000 until upstream's notifications migration took that
-- version in the merge (sqlx keys migrations by version, so two files may
-- never share one). Made idempotent so a fork database that applied the old
-- file can be repaired by dropping its `_sqlx_migrations` row and upgrading.
--
-- Personal API tokens: bearer auth on /api/* for bot/script accounts.
-- The secret is 32 CSPRNG bytes shown once at creation; only its SHA-256
-- hash is stored (unique index doubles as the lookup key). A token acts
-- as its user in the workspace it was minted in, so removal from the
-- workspace kills it (the role lookup fails, no AuthContext). Scopes are
-- a subset of read/create/write/delete.

CREATE TABLE IF NOT EXISTS api_tokens (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         text NOT NULL,
    token_hash   bytea NOT NULL UNIQUE,
    scopes       text[] NOT NULL CHECK (array_length(scopes, 1) >= 1),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    revoked_at   timestamptz
);
CREATE INDEX IF NOT EXISTS api_tokens_user_idx ON api_tokens (user_id, workspace_id) WHERE revoked_at IS NULL;