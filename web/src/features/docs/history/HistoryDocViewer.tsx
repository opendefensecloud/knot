import type { JSONContent } from "@tiptap/core";
import { EditorContent, useEditor } from "@tiptap/react";
import { useEffect, useMemo } from "react";
import { useNavigate } from "react-router-dom";

import { createSchemaExtensions } from "../../editor/extensions";
import { historyDiffExtensions } from "./HistoryDiffExtension";
import { viewerContent } from "./viewerContent";

/**
 * A past version, read-only, in the live editor's styles: the viewer's
 * schema (the live one plus the history marks, read-only boards and task
 * lists) over the version's ProseMirror JSON.
 *
 * It takes its content once: the version pane remounts it (by `key`) for
 * every version instead of calling `setContent`. The JSON is classified once
 * per version (`viewerContent`), not on every re-render of the pane, which
 * the poll, hover and focus cause; a version near the plain cap is several
 * MiB of JSON. A version the schema cannot parse renders nothing and calls
 * `onContentError`; one it parses but `check()` rejects renders as the live
 * editor shows it.
 */
export function HistoryDocViewer({ doc, onContentError }: { doc: unknown; onContentError: () => void }) {
  const c = useMemo(() => viewerContent(doc), [doc]);
  const invalid = c.mode === "invalid";
  useEffect(() => {
    if (invalid) onContentError();
  }, [invalid, onContentError]);
  if (c.mode === "invalid") return null;
  return <ReadOnlyEditor content={c.content} onContentError={onContentError} />;
}

function ReadOnlyEditor({ content, onContentError }: { content: JSONContent; onContentError: () => void }) {
  const navigate = useNavigate();
  const editor = useEditor({
    editable: false,
    extensions: [...createSchemaExtensions({ navigate, viewer: true }), ...historyDiffExtensions],
    content,
    // `viewerContent` already ran `check()`; lenient content is shown as is.
    enableContentCheck: false,
    onContentError: () => onContentError(),
  });
  // The live editor's host: `.ProseMirror` carries the prose styles.
  return (
    <div className="relative">
      <EditorContent editor={editor} />
    </div>
  );
}
