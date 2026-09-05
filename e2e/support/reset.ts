import { execSync } from "node:child_process";

/**
 * Wipe all tenant data between specs.
 *
 * Two competing constraints, and the shape below is what satisfies both.
 *
 * TRUNCATE is the right *operation*: it is atomic under one exclusive lock, so
 * a live CRDT room actor cannot insert a `doc_updates` row for a document that
 * is halfway deleted. Deleting row-by-row instead races those writers and
 * showers the server with foreign-key violations.
 *
 * But a bare TRUNCATE is what used to hang the suite. It needs ACCESS
 * EXCLUSIVE on every named table, room actors hold those tables busy for as
 * long as a document is open, and — the part that turns a wait into a wedge —
 * Postgres queues lock requests FIFO, so a *pending* TRUNCATE also blocks
 * every query that arrives after it. One reset could park the whole server
 * behind it for the rest of the run.
 *
 * `lock_timeout` is what makes that safe: if the lock is not free within a few
 * seconds the statement aborts and, crucially, stops queueing — the server is
 * immediately unblocked. We then back off and try again, which lets the rooms
 * drain. In practice the first attempt succeeds; the retries exist so a busy
 * moment costs a second rather than the entire run.
 */
const ATTEMPTS = 5;
const LOCK_TIMEOUT = "4s";

const TABLES = [
  "comment_reactions",
  "comments",
  "acl_invalidations",
  "audit_events",
  "blob_bytes",
  "blobs",
  "board_snapshots",
  "board_updates",
  "boards",
  "doc_markdown_cache",
  "doc_snapshots",
  "doc_tasks",
  "doc_updates",
  "document_grants",
  "documents",
  "share_tokens",
  "sessions",
  "workspace_members",
  "users",
  "workspaces",
].join(", ");

const SQL = `SET lock_timeout = '${LOCK_TIMEOUT}'; TRUNCATE TABLE ${TABLES} CASCADE`;

function isLockTimeout(detail: string): boolean {
  return detail.includes("lock_timeout") || detail.includes("canceling statement");
}

export function reset(): void {
  let last = "";
  for (let attempt = 1; attempt <= ATTEMPTS; attempt++) {
    try {
      execSync(
        `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -v ON_ERROR_STOP=1 -c "${SQL}"`,
        { cwd: "..", stdio: "pipe" },
      );
      return;
    } catch (e) {
      const detail = (e as { stderr?: Buffer }).stderr?.toString() ?? String(e);
      if (!isLockTimeout(detail)) throw e;
      last = detail;
      // Busy-wait rather than await: callers use this from sync beforeAll
      // hooks, and the whole point is to give the server a moment to finish
      // whatever it is holding.
      const until = Date.now() + attempt * 500;
      while (Date.now() < until) {
        /* back off */
      }
    }
  }
  throw new Error(
    `e2e reset could not lock the tenant tables after ${ATTEMPTS} attempts.\n` +
      "Something is holding a conflicting lock — usually a knot-server killed\n" +
      "mid-request, leaving connections Postgres still thinks are live.\n" +
      "Inspect with:\n" +
      "  SELECT pid, state, now()-xact_start AS xact_age, query\n" +
      "  FROM pg_stat_activity WHERE datname='knot' AND xact_start IS NOT NULL\n" +
      "  ORDER BY xact_start;\n" +
      "Clear with pg_terminate_backend(pid) and restart knot-server.\n\n" +
      last,
  );
}
