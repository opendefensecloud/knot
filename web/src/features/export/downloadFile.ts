import { fetchBlob } from "../../lib/api";
import { notifyFailure, type Notify } from "../../lib/limitErrors";
import { AUDIT_UNAVAILABLE_TEXT, isAuditUnavailable } from "./auditRefusal";
import { saveBlob } from "./exportFile";

/**
 * Downloads a file through `fetch` and saves it, rather than with an
 * `<a download>` navigation, so a refused download (a 429 from the export
 * rate limit, or a `503 audit.unavailable` for an export the audit log
 * couldn't record) is shown in the app instead of as a failed browser
 * download. Returns whether the file was saved.
 */
export async function downloadFile(opts: {
  path: string;
  fileName: string;
  notify: Notify;
  failureText: string;
}): Promise<boolean> {
  const result = await fetchBlob(opts.path);
  if ("error" in result) {
    if (isAuditUnavailable(result.error)) opts.notify("error", AUDIT_UNAVAILABLE_TEXT);
    else notifyFailure(opts.notify, result.error, opts.failureText);
    return false;
  }
  saveBlob(result.ok, opts.fileName);
  return true;
}
