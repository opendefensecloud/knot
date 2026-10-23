-- user_last_seen_version
-- Created 2026-10-23
--
-- The newest release whose notes the user has seen, for the "knot was
-- updated" card. Stored per user, not per browser, so someone on a laptop
-- and a phone sees the card once. Only ever moves forward (the server
-- compares versions as semver before writing).
--
-- NULL means "not set yet": the first GET /api/whats-new sets it to the
-- running version, so a user created from now on starts caught up instead
-- of being shown every release since 0.1.0.
ALTER TABLE users ADD COLUMN last_seen_version text NULL;

-- Everyone who exists at upgrade time has been using 0.8.1, the last release
-- without the card, so they get the card for the release that ships it.
-- Left NULL they would count as new users and never be told.
UPDATE users SET last_seen_version = '0.8.1';
