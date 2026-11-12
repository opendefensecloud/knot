-- Scoped API keys for humans, bots and AI agents, step 2 of 2.
--
-- Forward-only. Adds the api_keys table and six guard triggers on top of
-- users.kind (20261112120000_api_keys_users_kind). Keys are stored only as
-- HMAC-SHA256(KNOT_SESSION_KEY, secret); rotating the session key therefore
-- invalidates every session AND every API key.
--
-- sqlx runs this file in one transaction while the previous release keeps
-- serving. VALIDATE CONSTRAINT scans users under SHARE UPDATE EXCLUSIVE only,
-- which blocks no reader or writer, so it runs first, alone. Then
-- knot_lock_tables (20261107120000) takes SHARE ROW EXCLUSIVE, which the new
-- foreign keys and CREATE TRIGGER need, on all four tables in bounded
-- attempts, users (the busiest) first; it never waits for one table while
-- holding another, so it cannot deadlock with an app transaction that holds
-- one of them and asks for another. Once it holds them nothing below waits
-- for a lock the app holds: the statements only create the new, empty table
-- and its triggers. Writes to users, workspace_members and document_grants
-- wait until COMMIT; reads, edit saves and page loads go on.
SET LOCAL lock_timeout = '5s';

ALTER TABLE users VALIDATE CONSTRAINT users_kind_check;
ALTER TABLE users VALIDATE CONSTRAINT users_bot_has_no_credentials;

SELECT knot_lock_tables('20261112120001_api_keys_and_bots',
                        ARRAY['users', 'document_grants', 'workspace_members', 'workspaces']::regclass[],
                        'SHARE ROW EXCLUSIVE');

CREATE TABLE api_keys (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 100),
    -- Deterministic HMAC, so the UNIQUE index doubles as the lookup path.
    key_hash     bytea NOT NULL UNIQUE,
    scopes       text[] NOT NULL,
    created_by   uuid NOT NULL REFERENCES users(id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NULL,
    last_used_at timestamptz NULL,
    revoked_at   timestamptz NULL,
    CONSTRAINT api_keys_scopes_known CHECK (
        cardinality(scopes) BETWEEN 1 AND 4
        AND scopes <@ ARRAY['read','create','update','delete'])
);

CREATE INDEX api_keys_user_live_idx ON api_keys (user_id) WHERE revoked_at IS NULL;

-- ---------------------------------------------------------------------
-- Trigger functions
-- ---------------------------------------------------------------------

-- S5: a password change revokes all of the user's live keys, and the bot keys
-- that user minted: a stolen session that minted a bot key before the owner
-- reset the password must not outlive the reset. Lives in the DB so every
-- change of the password hash (the app, or a direct database update) is
-- covered.
--
-- Every revoking trigger here also writes an api-key.revoke-cascade audit row
-- per workspace, naming the cause and the keys: a revoked key answers exactly
-- like a typo, so the audit log is where an operator finds out why. No row
-- when nothing was revoked. actor_id stays NULL: the trigger can't know who
-- acted, and the member paths write their own row saying so.
CREATE OR REPLACE FUNCTION users_revoke_api_keys_on_password_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  WITH revoked AS (
    UPDATE api_keys SET revoked_at = now()
     WHERE (user_id = NEW.id OR created_by = NEW.id) AND revoked_at IS NULL
    RETURNING id, workspace_id
  )
  INSERT INTO audit_events (workspace_id, action, target_kind, target_id, data)
  SELECT workspace_id, 'api-key.revoke-cascade', 'user', NEW.id,
         jsonb_build_object('cause', 'password_change', 'key_ids', jsonb_agg(id))
    FROM revoked
   GROUP BY workspace_id;
  RETURN NULL;
END;
$$;

-- S4: a bot can never own the workspace. The app answers 422 first; this is
-- the backstop.
CREATE OR REPLACE FUNCTION workspace_members_bot_not_owner() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.role = 'owner'
     AND EXISTS (SELECT 1 FROM users WHERE id = NEW.user_id AND kind = 'bot') THEN
    RAISE EXCEPTION 'bot user % cannot be workspace owner', NEW.user_id
      USING ERRCODE = 'KB001';
  END IF;
  RETURN NEW;
END;
$$;

-- S4: a bot can never own a document. The app stores user principals in
-- canonical form, but any spelling of the bot's UUID counts here
-- (hyphenated, simple, braced, upper case, urn:uuid:), so a row written
-- some other way can't make a bot an owner either. The principal may be
-- 'group:...' or legacy text: a failed cast means "not a user", not an
-- error.
CREATE OR REPLACE FUNCTION document_grants_bot_not_owner() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  target uuid;
BEGIN
  IF NEW.role = 'owner' AND NEW.principal LIKE 'user:%' THEN
    BEGIN
      target := regexp_replace(substr(NEW.principal, 6), '^urn:uuid:', '', 'i')::uuid;
    EXCEPTION WHEN invalid_text_representation THEN
      target := NULL;
    END;
    IF target IS NOT NULL
       AND EXISTS (SELECT 1 FROM users WHERE id = target AND kind = 'bot') THEN
      RAISE EXCEPTION 'bot principal % cannot be document owner', NEW.principal
        USING ERRCODE = 'KB002';
    END IF;
  END IF;
  RETURN NEW;
END;
$$;

-- S6: a key must expire within 90 days when its user or its minter is a
-- pure-OIDC human (no password, OIDC identity): S5 revokes keys on a password
-- change, and such a human has no password to change. Enforced in the DB so no
-- app path can mint an unbounded key. Exactly 90 days is allowed (strict >). A
-- bot key minted by a human with a password stays uncapped, since S5 can revoke
-- it. principal_kind in routes/api/api_keys.rs holds the same pure-OIDC rule:
-- change both together.
CREATE OR REPLACE FUNCTION api_keys_sso_ttl_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM users
              WHERE id IN (NEW.user_id, NEW.created_by)
                AND kind = 'human'
                AND password_hash IS NULL
                AND oidc_subject IS NOT NULL)
     AND (NEW.expires_at IS NULL
          OR NEW.expires_at > now() + interval '90 days') THEN
    RAISE EXCEPTION 'api key of user %, minted by %, must expire within 90 days: an OIDC-only user or minter has no password change to revoke it',
      NEW.user_id, NEW.created_by
      USING ERRCODE = 'KB003';
  END IF;
  RETURN NEW;
END;
$$;

-- S8: leaving the workspace revokes the member's keys there, so re-adding
-- the user does not bring them back, and revokes the keys the member minted
-- for others (bots), so a departed owner keeps no bot access. Losing the
-- owner role revokes the keys the user minted for others as well.
CREATE OR REPLACE FUNCTION workspace_members_revoke_api_keys() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    WITH revoked AS (
      UPDATE api_keys SET revoked_at = now()
       WHERE workspace_id = OLD.workspace_id
         AND (user_id = OLD.user_id OR created_by = OLD.user_id)
         AND revoked_at IS NULL
      RETURNING id
    )
    INSERT INTO audit_events (workspace_id, action, target_kind, target_id, data)
    SELECT OLD.workspace_id, 'api-key.revoke-cascade', 'user', OLD.user_id,
           jsonb_build_object('cause', 'member_removed', 'key_ids', jsonb_agg(id))
      FROM revoked
    HAVING count(*) > 0;
  ELSIF OLD.role = 'owner' AND NEW.role <> 'owner' THEN
    WITH revoked AS (
      UPDATE api_keys SET revoked_at = now()
       WHERE workspace_id = OLD.workspace_id
         AND created_by = OLD.user_id
         AND user_id <> OLD.user_id
         AND revoked_at IS NULL
      RETURNING id
    )
    INSERT INTO audit_events (workspace_id, action, target_kind, target_id, data)
    SELECT OLD.workspace_id, 'api-key.revoke-cascade', 'user', OLD.user_id,
           jsonb_build_object('cause', 'owner_demoted', 'key_ids', jsonb_agg(id))
      FROM revoked
    HAVING count(*) > 0;
  END IF;
  RETURN NULL;
END;
$$;

-- S4: users.kind never changes, so no human owner (or holder of an owner
-- grant) can be turned into a bot owner, and no bot into a human that could
-- then be given credentials.
CREATE OR REPLACE FUNCTION users_kind_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'users.kind of % cannot change', OLD.id
    USING ERRCODE = 'KB004';
END;
$$;

-- ---------------------------------------------------------------------
-- Triggers (no DROP TRIGGER IF EXISTS: it takes ACCESS EXCLUSIVE on the table
-- even when the trigger is absent, which would undo the lock plan above)
-- ---------------------------------------------------------------------

CREATE TRIGGER users_revoke_api_keys_on_password_change
  AFTER UPDATE OF password_hash ON users
  FOR EACH ROW
  WHEN (OLD.password_hash IS DISTINCT FROM NEW.password_hash)
  EXECUTE FUNCTION users_revoke_api_keys_on_password_change();

CREATE TRIGGER workspace_members_bot_not_owner
  BEFORE INSERT OR UPDATE ON workspace_members
  FOR EACH ROW EXECUTE FUNCTION workspace_members_bot_not_owner();

CREATE TRIGGER document_grants_bot_not_owner
  BEFORE INSERT OR UPDATE OF role, principal ON document_grants
  FOR EACH ROW EXECUTE FUNCTION document_grants_bot_not_owner();

CREATE TRIGGER api_keys_sso_ttl_guard
  BEFORE INSERT OR UPDATE OF expires_at, user_id, created_by ON api_keys
  FOR EACH ROW EXECUTE FUNCTION api_keys_sso_ttl_guard();

CREATE TRIGGER workspace_members_revoke_api_keys
  AFTER DELETE OR UPDATE OF role ON workspace_members
  FOR EACH ROW EXECUTE FUNCTION workspace_members_revoke_api_keys();

CREATE TRIGGER users_kind_immutable
  BEFORE UPDATE OF kind ON users
  FOR EACH ROW
  WHEN (OLD.kind IS DISTINCT FROM NEW.kind)
  EXECUTE FUNCTION users_kind_immutable();
