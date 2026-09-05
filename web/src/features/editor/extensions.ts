import Collaboration from "@tiptap/extension-collaboration";
import CollaborationCursor from "@tiptap/extension-collaboration-cursor";
import Image from "@tiptap/extension-image";
import Link from "@tiptap/extension-link";
import Underline from "@tiptap/extension-underline";
import StarterKit from "@tiptap/starter-kit";
import type { Awareness } from "y-protocols/awareness";
import type * as Y from "yjs";
import type { NavigateFunction } from "react-router-dom";

import { Attachment } from "./nodes/AttachmentNode";
import { ExcalidrawBoard } from "./nodes/ExcalidrawBoard";
import { MermaidCodeBlock } from "./nodes/MermaidCodeBlock";
import { CommentsHighlightExtension } from "./CommentsHighlightExtension";
import {
  KnotBulletList,
  KnotHardBreak,
  KnotHorizontalRule,
  KnotListItem,
  KnotOrderedList,
} from "./SchemaNameOverrides";
import { TaskListExtension } from "./TaskListExtension";
import { InternalLinkExtension } from "./InternalLinkExtension";
import { MentionExtension, type MentionMember } from "./MentionExtension";
import { DateTimeExtension } from "./DateTimeExtension";
import {
  KnotTable,
  KnotTableRow,
  KnotTableCell,
  KnotTableHeader,
} from "./TableExtensions";

/**
 * Tiptap's Link ships href/target/rel/class but no `title`, while
 * `tools/schema.json` declares one and both `from_markdown` and `to_markdown`
 * honour it. ProseMirror drops attributes its schema does not declare, so an
 * imported `[text](url "title")` lost its title the first time anyone edited
 * the document — in storage, for every reader, with no error.
 *
 * `renderHTML` returns nothing when the title is unset so bare links keep
 * emitting exactly the markup (and markdown) they always have.
 */
const KnotLink = Link.extend({
  addAttributes() {
    return {
      ...this.parent?.(),
      title: {
        default: null,
        parseHTML: (el: HTMLElement) => el.getAttribute("title"),
        renderHTML: (attrs: { title?: string | null }) =>
          attrs.title ? { title: attrs.title } : {},
      },
    };
  },
});

/** Canonical Tiptap extension set that matches the server schema generated
 *  from `tools/schema.json`. History is disabled because Yjs UndoManager
 *  owns undo. */
export function createExtensions(opts: {
  doc: Y.Doc;
  awareness: Awareness;
  user: { name: string; color: string };
  onSelectComment?: (commentId: string) => void;
  navigate?: NavigateFunction;
  fetchMembers?: () => Promise<MentionMember[]>;
}) {
  return [
    StarterKit.configure({
      history: false,
      codeBlock: false,
      // Disable the camelCase node defaults; we re-add snake_case versions
      // below so the Y.XmlFragment matches our canonical schema.
      bulletList: false,
      orderedList: false,
      listItem: false,
      hardBreak: false,
      horizontalRule: false,
    }),
    KnotBulletList,
    KnotOrderedList,
    KnotListItem,
    KnotHardBreak,
    KnotHorizontalRule,
    MermaidCodeBlock,
    Underline,
    KnotLink.configure({
      openOnClick: false,
      autolink: true,
      HTMLAttributes: { rel: "noopener noreferrer", target: "_blank" },
    }),
    Collaboration.configure({ document: opts.doc }),
    CollaborationCursor.configure({
      provider: { awareness: opts.awareness } as never,
      user: opts.user,
    }),
    Image.configure({ inline: false, allowBase64: false }),
    Attachment,
    ExcalidrawBoard,
    TaskListExtension,
    InternalLinkExtension.configure({ navigate: opts.navigate ?? null }),
    MentionExtension.configure({
      fetchMembers: opts.fetchMembers ?? (() => Promise.resolve([])),
    }),
    DateTimeExtension,
    KnotTable,
    KnotTableRow,
    KnotTableCell,
    KnotTableHeader,
    CommentsHighlightExtension.configure({
      doc: opts.doc,
      comments: [],
      activeCommentId: null,
      onSelect: opts.onSelectComment ?? (() => {}),
    }),
  ];
}
