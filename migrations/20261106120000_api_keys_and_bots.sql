-- Scoped API keys for humans, bots and AI agents.
--
-- Forward-only. Adds users.kind, the api_keys table and six guard triggers.
-- Keys are stored only as HMAC-SHA256(KNOT_SESSION_KEY, secret); rotating the
-- session key therefore invalidates every session AND every API key.

-- The ALTERs below lock users, and CREATE TRIGGER locks workspace_members and
-- document_grants: fail fast behind a long transaction rather than queue all
-- authenticated traffic. sqlx runs this file in one transaction.
SET LOCAL lock_timeout = '5s';

-- Constant DEFAULT: metadata-only column add, no table rewrite.
ALTER TABLE users ADD COLUMN kind text NOT NULL DEFAULT 'human'
    CHECK (kind IN ('human','bot'));

-- A bot can never log in interactively: no password, no OIDC identity.
ALTER TABLE users ADD CONSTRAINT users_bot_has_no_credentials
    CHECK (kind <> 'bot' OR (password_hash IS NULL AND oidc_subject IS NULL));

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
CREATE OR REPLACE FUNCTION users_revoke_api_keys_on_password_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  UPDATE api_keys SET revoked_at = now()
   WHERE (user_id = NEW.id OR created_by = NEW.id) AND revoked_at IS NULL;
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

-- S6: pure-OIDC humans (no password, OIDC identity) get keys of at most 90
-- days. Enforced in the DB so no app path can mint an unbounded key for an
-- account whose real lifetime is controlled by the IdP. Exactly 90 days is
-- allowed (strict >). Bots (kind 'bot') are exempt and may have NULL expiry.
CREATE OR REPLACE FUNCTION api_keys_sso_ttl_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM users
              WHERE id = NEW.user_id
                AND kind = 'human'
                AND password_hash IS NULL
                AND oidc_subject IS NOT NULL)
     AND (NEW.expires_at IS NULL
          OR NEW.expires_at > now() + interval '90 days') THEN
    RAISE EXCEPTION 'api key for OIDC-only user % must expire within 90 days', NEW.user_id
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
    UPDATE api_keys SET revoked_at = now()
     WHERE workspace_id = OLD.workspace_id
       AND (user_id = OLD.user_id OR created_by = OLD.user_id)
       AND revoked_at IS NULL;
  ELSIF OLD.role = 'owner' AND NEW.role <> 'owner' THEN
    UPDATE api_keys SET revoked_at = now()
     WHERE workspace_id = OLD.workspace_id
       AND created_by = OLD.user_id
       AND user_id <> OLD.user_id
       AND revoked_at IS NULL;
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
-- Triggers (DROP + CREATE so a later CREATE OR REPLACE FUNCTION takes effect)
-- ---------------------------------------------------------------------

DROP TRIGGER IF EXISTS users_revoke_api_keys_on_password_change ON users;
CREATE TRIGGER users_revoke_api_keys_on_password_change
  AFTER UPDATE OF password_hash ON users
  FOR EACH ROW
  WHEN (OLD.password_hash IS DISTINCT FROM NEW.password_hash)
  EXECUTE FUNCTION users_revoke_api_keys_on_password_change();

DROP TRIGGER IF EXISTS workspace_members_bot_not_owner ON workspace_members;
CREATE TRIGGER workspace_members_bot_not_owner
  BEFORE INSERT OR UPDATE ON workspace_members
  FOR EACH ROW EXECUTE FUNCTION workspace_members_bot_not_owner();

DROP TRIGGER IF EXISTS document_grants_bot_not_owner ON document_grants;
CREATE TRIGGER document_grants_bot_not_owner
  BEFORE INSERT OR UPDATE OF role, principal ON document_grants
  FOR EACH ROW EXECUTE FUNCTION document_grants_bot_not_owner();

DROP TRIGGER IF EXISTS api_keys_sso_ttl_guard ON api_keys;
CREATE TRIGGER api_keys_sso_ttl_guard
  BEFORE INSERT OR UPDATE OF expires_at, user_id ON api_keys
  FOR EACH ROW EXECUTE FUNCTION api_keys_sso_ttl_guard();

DROP TRIGGER IF EXISTS workspace_members_revoke_api_keys ON workspace_members;
CREATE TRIGGER workspace_members_revoke_api_keys
  AFTER DELETE OR UPDATE OF role ON workspace_members
  FOR EACH ROW EXECUTE FUNCTION workspace_members_revoke_api_keys();

DROP TRIGGER IF EXISTS users_kind_immutable ON users;
CREATE TRIGGER users_kind_immutable
  BEFORE UPDATE OF kind ON users
  FOR EACH ROW
  WHEN (OLD.kind IS DISTINCT FROM NEW.kind)
  EXECUTE FUNCTION users_kind_immutable();
