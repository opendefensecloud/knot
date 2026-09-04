import { FileUp } from "lucide-react";
import { useRef, useState } from "react";

import { IconButton } from "../../components/ui/IconButton";
import { historyApi } from "../../lib/history.api";
import { useUi } from "../../stores/ui";

import { docsApi } from "./docs.api";

/** The server caps the import body at 1 MB (`markdown.rs`, `to_bytes`).
 *  Anything larger is refused by axum before our handler runs, producing a
 *  bare `400 bad_request` — check it here so the user gets a real reason. */
const MAX_IMPORT_BYTES = 1024 * 1024;

function messageFor(code: string): string {
  switch (code) {
    case "acl.editor_required":
    case "acl.no_grant":
      return "You don't have permission to import into this page.";
    case "markdown.not_utf8":
      return "That file isn't valid UTF-8 text.";
    case "markdown.parse":
      return "Couldn't read that file as Markdown.";
    default:
      return "Import failed.";
  }
}

/**
 * "Import Markdown…" — reads a local `.md` file and replaces this page's
 * body with it.
 *
 * Import replaces rather than appends, because the endpoint's default merges
 * into the existing content and would duplicate a page you have already
 * used. The old body is still reachable from History, which is what the
 * confirm says.
 */
export function ImportMarkdownButton({
  docId,
  docTitle,
}: {
  docId: string;
  docTitle: string;
}) {
  const inputRef = useRef<HTMLInputElement | null>(null);
  const notify = useUi((s) => s.notify);
  const [busy, setBusy] = useState(false);

  async function importFile(file: File) {
    if (file.size > MAX_IMPORT_BYTES) {
      notify("error", "That file is larger than the 1 MB import limit.");
      return;
    }
    const markdown = await file.text();

    // Only warn when there is something to lose. If the current body can't
    // be read, ask anyway rather than assume the page is empty.
    const current = await historyApi.exportMarkdown(docId);
    const isEmpty = "ok" in current && current.ok.trim().length === 0;
    if (!isEmpty) {
      const proceed = window.confirm(
        `Replace the contents of "${docTitle}" with ${file.name}? ` +
          "The current version stays in History.",
      );
      if (!proceed) return;
    }

    setBusy(true);
    const r = await docsApi.importMarkdown(docId, markdown, "replace");
    setBusy(false);
    if ("error" in r) {
      notify("error", messageFor(r.error.code));
      return;
    }
    // No refetch needed: the room actor fans the replace out over the same
    // WebSocket the editor is already on, exactly as a history restore does.
    notify("info", `Imported "${file.name}"`);
  }

  return (
    <>
      <IconButton
        data-testid="doc-import-md"
        label="Import Markdown…"
        disabled={busy}
        onClick={() => inputRef.current?.click()}
      >
        <FileUp size={16} aria-hidden />
      </IconButton>
      <input
        ref={inputRef}
        type="file"
        accept=".md,.markdown,text/markdown,text/plain"
        className="hidden"
        data-testid="doc-import-md-input"
        onChange={(e) => {
          const file = e.target.files?.[0];
          // Reset first so picking the same file again still fires onChange.
          e.target.value = "";
          if (file) void importFile(file);
        }}
      />
    </>
  );
}
