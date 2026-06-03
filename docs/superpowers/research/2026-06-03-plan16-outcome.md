# Plan 16 Outcome — Prefix Search

**Status:** GO. Half-day plan, two commits, three new integration cases + four unit tests.

**Verdict:** Searching for "find" now matches "Findable". The Plan 14 UX wart is gone.

## What landed

| Commit | Subject |
|---|---|
| 3b5649f | feat(knot-storage): prefix-match search via to_tsquery + :* + sanitizer |
| ec359a6 | test(knot-server): prefix search + special-char safety + multi-word AND |

## Gates

- `cargo test --workspace` — 9/9 search integration cases (was 6) + 4 new unit tests for the sanitizer
- `cargo clippy -p knot-storage --all-targets -- -D warnings` — clean
- `pnpm playwright test` — search.spec.ts + command-palette.spec.ts pass unchanged
- Full suite remains 22/22

## Implementation

Two changes in `crates/knot-storage/src/search.rs`:

1. **`to_prefix_tsquery(raw)` sanitizer.** Splits on whitespace, strips every character except `[A-Za-z0-9_]` from each token, drops tokens under 2 chars, appends `:*` to each, joins with ` & `, caps at 8 clauses. Returns `None` when nothing survives — the handler then skips the DB call.

   ```rust
   to_prefix_tsquery("find world")     // Some("find:* & world:*")
   to_prefix_tsquery("'; DROP TABLE")  // Some("DROP:* & TABLE:*")
   to_prefix_tsquery("a b c")          // None  (all too short)
   to_prefix_tsquery("")               // None
   ```

2. **`plainto_tsquery` → `to_tsquery` throughout the SQL.** Same parameter binding; the value bound is now the sanitized tsquery expression instead of the raw input.

## What was non-obvious

**`to_tsquery` is operator-syntax-sensitive.** Without the sanitizer, a single unescaped apostrophe or `&` would 500 the endpoint. The sanitizer strips operator-significant characters from each token before assembling the expression. Apostrophes in `'; DROP TABLE` get stripped; the remaining `DROP TABLE` becomes `DROP:* & TABLE:*`. Safe and predictable.

**The 8-clause cap is a planner-time defense.** A user pasting a 200-word query (e.g. by accident) would otherwise build a 200-term `AND` chain, which the GIN index handles but the rank computation makes expensive. Truncate before binding.

**Token length floor stays at 2.** Lower (1) would match every doc with `e:*` against an English corpus — useless. The frontend's separate `q.length >= 2` short-circuit means the floor isn't user-visible, but it's defensive for any future caller.

## What's still deferred

- **Phrase search.** `"hello world"` (quoted) would map to `hello <-> world` (followed-by) — needs quote detection in the sanitizer. Not requested.
- **Operator UI.** Power-user `AND`/`OR`/`NOT` syntax. Out of scope.
- **Stop-word handling.** "the find" currently emits `the:* & find:*`. The `english` config treats `the` as a stop word in the index, so the AND still matches docs containing only `find`. No fix needed.

## Carryforward

Continue with **Plan 15 (mobile / responsive)** next — affects every component, so doing it before the bigger feature plans (17 share links, 19 comments) means new UI is responsive from the start.

## Files

| Path | Role |
|---|---|
| `crates/knot-storage/src/search.rs` | `to_prefix_tsquery` + `to_tsquery` SQL + 4 unit tests |
| `crates/knot-server/tests/search_integration.rs` | +3 cases (prefix, special-char safety, multi-word AND) |
