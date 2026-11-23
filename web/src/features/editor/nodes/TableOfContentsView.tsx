import { NodeViewWrapper, type ReactNodeViewProps } from "@tiptap/react";
import { useEffect, useState } from "react";

import { tocEntries, type TocEntry } from "../headingAnchors";
import { scrollToHeading } from "../scrollToHeading";
import { clampTocLevel } from "./TableOfContents";

const LEVELS = [1, 2, 3, 4, 5, 6];

/**
 * The table of contents box. The list is read from the editor's document
 * on every transaction (a collaborator's edit too), so it follows the
 * headings without ever writing to the document; only the depth control
 * changes the node, through a normal edit.
 */
export function TableOfContentsView({ node, editor, getPos, updateAttributes }: ReactNodeViewProps) {
  // Bumped on every transaction, so the list below is read again from the
  // document whenever it may have changed.
  const [, setVersion] = useState(0);
  useEffect(() => {
    const bump = () => setVersion((v) => v + 1);
    editor.on("transaction", bump);
    editor.on("update", bump);
    return () => {
      editor.off("transaction", bump);
      editor.off("update", bump);
    };
  }, [editor]);
  // The node as the document has it now: the `node` prop can lag a
  // transaction behind.
  const pos = getPos();
  const live = typeof pos === "number" ? editor.state.doc.nodeAt(pos) : null;
  const maxLevel = clampTocLevel((live?.type.name === "table_of_contents" ? live : node).attrs.maxLevel);
  const entries: TocEntry[] = tocEntries(editor.state.doc, maxLevel);
  const editable = editor.isEditable;

  // A read-only jump makes its heading focusable; once the page can be
  // edited that would take the focus away from the editor on a click into
  // the heading, so it goes.
  useEffect(() => {
    if (!editable) return;
    editor.view.dom
      .querySelectorAll<HTMLElement>("[data-toc-focus]")
      .forEach((el) => {
        el.removeAttribute("tabindex");
        el.removeAttribute("data-toc-focus");
      });
  }, [editor, editable]);

  /** Scrolls to the entry's heading and moves the focus there: the caret
   *  to the heading's start when the page can be edited, the heading
   *  itself (made focusable for it) when it cannot. The heading is looked
   *  up again in the live document: a collaborator's edit may have moved it
   *  since this list was drawn. */
  const jumpTo = (entry: TocEntry) => {
    const live = tocEntries(editor.state.doc, maxLevel).find((e) => e.slug === entry.slug);
    if (!live) return;
    const heading = scrollToHeading(live.slug, editor.view.dom);
    if (!heading) return;
    window.history.replaceState(window.history.state, "", `#${live.slug}`);
    if (editable) {
      editor.commands.setTextSelection(live.pos + 1);
      editor.view.focus();
    } else {
      heading.tabIndex = -1;
      heading.setAttribute("data-toc-focus", "");
      heading.addEventListener(
        "blur",
        () => {
          heading.removeAttribute("tabindex");
          heading.removeAttribute("data-toc-focus");
        },
        { once: true },
      );
      heading.focus({ preventScroll: true });
    }
  };

  return (
    <NodeViewWrapper
      as="nav"
      aria-label="Contents"
      data-testid="toc-box"
      data-knot-toc="true"
      data-history-change={(node.attrs.historyChange as string | null | undefined) ?? undefined}
      className="knot-box"
      contentEditable={false}
    >
      <div className="knot-box__head">
        <span className="knot-box__title">Contents</span>
        {editable && (
          <label className="knot-box__control">
            Levels
            <select
              data-testid="toc-levels"
              value={maxLevel}
              onChange={(e) => updateAttributes({ maxLevel: clampTocLevel(e.target.value) })}
            >
              {LEVELS.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      {entries.length === 0 ? (
        <p className="knot-box__empty">No headings yet</p>
      ) : (
        <ul className="knot-box__list">
          {entries.map((entry, i) => (
            <li key={`${entry.slug}-${i}`} className={`knot-box__l${entry.level}`}>
              <a
                href={`#${entry.slug}`}
                data-testid="toc-entry"
                onClick={(e) => {
                  e.preventDefault();
                  jumpTo(entry);
                }}
              >
                {entry.text}
              </a>
            </li>
          ))}
        </ul>
      )}
    </NodeViewWrapper>
  );
}
