/**
 * Exports fail closed: the server sends a file only once the export is
 * recorded in the audit log, and answers `503 audit.unavailable` with no
 * file when it can't record it (`audit_unavailable` in
 * crates/knot-server/src/http_error.rs). Every export path (docx/odt and
 * the Markdown zips) says so the same way.
 */

export const AUDIT_UNAVAILABLE_CODE = "audit.unavailable";

export const AUDIT_UNAVAILABLE_TEXT = "Export blocked: it couldn't be recorded in the audit log. Try again later.";

export function isAuditUnavailable(error: { code: string } | null | undefined): boolean {
  return error?.code === AUDIT_UNAVAILABLE_CODE;
}
