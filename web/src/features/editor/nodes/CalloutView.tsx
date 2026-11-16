import { NodeViewContent, NodeViewWrapper, type ReactNodeViewProps } from "@tiptap/react";
import { useState } from "react";

import { CALLOUT_ICON, CALLOUT_KINDS, CALLOUT_LABEL, calloutKind, normaliseTitle } from "../callouts";

/**
 * A blockquote. With a known `kind` it is a callout box: a header with the
 * kind's icon, its title (a plain-text field, the kind's name as
 * placeholder) and, when the page can be edited, a select for the kind;
 * the quote's blocks below. Without one it is a plain quote, as before.
 */
export function CalloutView({ node, editor, updateAttributes }: ReactNodeViewProps) {
  const kind = calloutKind(node.attrs.kind);
  const stored = typeof node.attrs.title === "string" ? node.attrs.title : "";
  // What the field shows: the stored title, unless the user is typing a
  // version of it (a trailing space, say) that stores as the same title.
  const [draft, setDraft] = useState(stored);
  const [seen, setSeen] = useState(stored);
  if (seen !== stored) {
    // The title changed on the node (a collaborator, undo).
    setSeen(stored);
    if (normaliseTitle(draft) !== stored) setDraft(stored);
  }
  const history = (node.attrs.historyChange as string | null | undefined) ?? undefined;

  if (!kind) {
    return (
      <NodeViewWrapper as="blockquote" data-history-change={history}>
        <NodeViewContent />
      </NodeViewWrapper>
    );
  }
  const Icon = CALLOUT_ICON[kind];
  const editable = editor.isEditable;
  return (
    <NodeViewWrapper
      as="blockquote"
      data-testid="callout"
      data-kind={kind}
      data-history-change={history}
      className={`knot-callout knot-callout--${kind}`}
    >
      <div className="knot-callout__head" contentEditable={false}>
        <Icon className="knot-callout__icon" aria-hidden="true" />
        {editable ? (
          <input
            data-testid="callout-title"
            className="knot-callout__title"
            aria-label="Callout title"
            placeholder={CALLOUT_LABEL[kind]}
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              updateAttributes({ title: normaliseTitle(e.target.value) || null });
            }}
          />
        ) : (
          <span data-testid="callout-title" className="knot-callout__title">
            {stored || CALLOUT_LABEL[kind]}
          </span>
        )}
        {editable && (
          <select
            data-testid="callout-kind"
            className="knot-callout__kind"
            aria-label="Callout kind"
            value={kind}
            onChange={(e) => updateAttributes({ kind: calloutKind(e.target.value) ?? kind })}
          >
            {CALLOUT_KINDS.map((k) => (
              <option key={k} value={k}>
                {CALLOUT_LABEL[k]}
              </option>
            ))}
          </select>
        )}
      </div>
      <NodeViewContent className="knot-callout__body" />
    </NodeViewWrapper>
  );
}
