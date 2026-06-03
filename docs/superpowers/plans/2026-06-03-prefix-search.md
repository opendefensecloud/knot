# Prefix Search Implementation Plan (Plan 16)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans (4-task micro-plan).

**Goal:** Make `"find"` match `"Findable"` in the command palette. Plan 14 used `plainto_tsquery` which is whole-word — `"find"` and `"findable"` stem to different lemmas. Switching to `to_tsquery` with a `:*` suffix gives prefix matching. The query string must be sanitized first because `to_tsquery` is operator-syntax-sensitive (`'foo & bar | !baz'`) — naive user input crashes the parser.

**Predecessor:** Plan 12.5 (HEAD `571f7a3`).

---

## Tasks

| # | Title | LOC ≈ |
|---|---|---|
| 1 | Add tsquery sanitizer + switch to to_tsquery with :* | 80 |
| 2 | Server integration tests — prefix + special chars + multi-word | 140 |
| 3 | Bump min query length to 2 chars (stays) + update palette debounce comment | 5 |
| 4 | Outcome doc | 0 |

---

## Task 1: Sanitizer + prefix tsquery

**Files:**
- Modify: `crates/knot-storage/src/search.rs`

Approach:
1. Sanitize: split the user input on whitespace, drop any token under 2 chars, strip every char except `[A-Za-z0-9_]` from each token (collapses unicode/punctuation), then join with ` & ` and append `:*` to each term.
2. Empty result after sanitization → return empty list, no DB hit.
3. SQL changes: every `plainto_tsquery('english', $2)` → `to_tsquery('english', $2)`. The `$2` value is now the pre-sanitized query string with `:*` suffixes.

Sketch:

```rust
fn to_prefix_tsquery(raw: &str) -> Option<String> {
    let mut tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| t.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("{t}:*"))
        .collect();
    if tokens.is_empty() { return None; }
    // Limit to a reasonable number of clauses; users with 50-word queries get truncated.
    tokens.truncate(8);
    Some(tokens.join(" & "))
}
```

In `PgSearchStore::search`, before the SQL call:

```rust
let Some(ts) = to_prefix_tsquery(q) else { return Ok(vec![]); };
```

Replace every `plainto_tsquery('english', $2)` with `to_tsquery('english', $2)`, and bind `ts` instead of the raw `q`.

Verify:

```bash
cd /home/nik/Development/knot
cargo check -p knot-storage
cargo clippy -p knot-storage --all-targets -- -D warnings
cargo nextest run -p knot-storage
```

Commit:

```bash
git add crates/knot-storage/
git commit -m "feat(knot-storage): prefix-match search via to_tsquery + :* + sanitizer"
```

---

## Task 2: Integration tests

**Files:**
- Modify: `crates/knot-server/tests/search_integration.rs`

Add cases:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn prefix_match_finds_doc_by_word_start() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Findable World").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "find").await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "prefix 'find' should match 'Findable'");
}

#[tokio::test(flavor = "multi_thread")]
async fn special_chars_dont_crash_query() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello world").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    // tsquery would explode on these without sanitization.
    for needle in ["!@#$%", "foo & bar", "'; DROP TABLE", "a:* | b"] {
        let (status, _) = do_search(&app, &sid, needle).await;
        assert_eq!(status, StatusCode::OK, "query '{needle}' should not 500");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_word_prefix_is_AND() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Alphabet Soup").await;
    make_doc(&state, ws, uid, "Beta Snack").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    // Both prefixes must match the same doc.
    let (_, body) = do_search(&app, &sid, "alph soup").await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Alphabet Soup");
}
```

Run + commit:

```bash
cargo nextest run -p knot-server --test search_integration
git add crates/knot-server/tests/
git commit -m "test(knot-server): prefix search + special-char safety + multi-word AND"
```

---

## Task 3: Touch the debounce comment

**Files:**
- Modify: `web/src/components/CommandPalette.tsx`

The palette currently bails when `q.length < 2`. Now that prefix matching exists, 2 chars actually returns useful hits. Update the comment if there is one (or leave the threshold; the bail-on-short still makes sense for performance — 1 char would match everything).

No behavior change. If no comment exists, skip the commit.

---

## Task 4: Outcome doc

Short outcome doc + Plan 16 row in `docs/superpowers/README.md`.

```bash
git add docs/
git commit -m "docs: Plan 16 outcome — prefix search"
```

---

## Self-review

- [ ] `cargo test --workspace` green (+3 new search cases)
- [ ] `pnpm playwright test` still 22/22 (the Plan 14 spec already uses "findable" so still passes; palette test still uses "findable" too)
- [ ] Manual: `make dev`, ⌘K, type "find" → matches a doc titled "Findable"
- [ ] Manual: `make dev`, ⌘K, type "'; DROP TABLE" → no error, empty results
