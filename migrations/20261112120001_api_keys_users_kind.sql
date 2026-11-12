-- Scoped API keys, step 1 of 2: users.kind and its checks.
--
-- Forward-only. sqlx runs this file in one transaction, as the Helm
-- pre-upgrade Job, while the previous release keeps serving. ADD COLUMN needs
-- ACCESS EXCLUSIVE on users, which stops every sign-in and every edit save
-- (doc_updates' foreign key reads users) until COMMIT, so this file does
-- nothing else and nothing slow: a constant DEFAULT rewrites no table, and
-- the two CHECKs are NOT VALID, which scans nothing (step 2 validates them
-- under a lock that blocks no one). The previous release never names the
-- column, so its statements keep working.
--
-- knot_lock_tables (20261107120000) takes the lock in bounded attempts and
-- never waits for one table while holding another; if a long transaction on
-- users keeps it from the lock it gives up after 45 s, naming the holder, and
-- the Job retries.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261112120001_api_keys_users_kind',
                        ARRAY['users']::regclass[],
                        'ACCESS EXCLUSIVE');

ALTER TABLE users ADD COLUMN kind text NOT NULL DEFAULT 'human';
ALTER TABLE users ADD CONSTRAINT users_kind_check
    CHECK (kind IN ('human','bot')) NOT VALID;

-- A bot can never log in interactively: no password, no OIDC identity.
ALTER TABLE users ADD CONSTRAINT users_bot_has_no_credentials
    CHECK (kind <> 'bot' OR (password_hash IS NULL AND oidc_subject IS NULL)) NOT VALID;
