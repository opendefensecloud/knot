# Invite: set display name — design (item B)

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

Inviting a user cannot set their display name. When an invite creates a brand-new
local account, the backend derives the display name from the email prefix
(`workspace.rs`: `let display = body.email.split('@').next()...`). The invite form
(`MembersPage.tsx`), the API client (`workspace.api.ts` `invite`), and the request
struct (`InviteRequest`) have no display-name field at all. There is also no later
self-service profile edit, so the invited user is stuck with the email-prefix name.

## Goal

Let the inviting owner optionally set the new user's display name. Non-breaking:
when omitted, keep today's email-prefix fallback.

## Design

End-to-end optional `display_name`, applied **only when the invite creates a new
local user** (an existing user found by email keeps their current name — invites
don't rename people).

- **Backend** (`crates/knot-server/src/routes/api/workspace.rs`): add
  `display_name: Option<String>` to `InviteRequest`. In the `create_local` branch,
  choose the display name as: the request's `display_name`, trimmed, if non-empty;
  otherwise the email prefix (unchanged fallback).
- **Frontend API** (`web/src/features/workspace/workspace.api.ts`): add a
  `displayName?: string` parameter to `invite(...)`; include `display_name` in the
  POST body only when non-empty.
- **Frontend UI** (`web/src/features/workspace/MembersPage.tsx`): add an
  `inviteName` state and a "Display name (optional)" text input
  (`data-testid="invite-display-name"`) to the invite form; pass it to the invite
  call and clear it on success.

## Testing

- **Backend** (`crates/knot-server/tests/workspace_invite_password_integration.rs`):
  invite a new email **with** `display_name` + password → the created user's
  `display_name` equals the provided value; invite another new email **without**
  `display_name` → falls back to the email prefix.
- **Frontend:** typecheck; the change is mechanical. (No dedicated MembersPage unit
  test exists; not adding one is proportionate.)

## Out of scope

Self-service profile/display-name editing for existing users (separate feature).
