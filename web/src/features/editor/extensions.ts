import Collaboration from "@tiptap/extension-collaboration";
import CollaborationCaret from "@tiptap/extension-collaboration-caret";
import Image from "@tiptap/extension-image";
import Link from "@tiptap/extension-link";
import Underline from "@tiptap/extension-underline";
import { Plugin, type Transaction } from "@tiptap/pm/state";
import StarterKit from "@tiptap/starter-kit";
import { ySyncPluginKey } from "@tiptap/y-tiptap";
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

  /**
   * Autolink only the local user's own typing.
   *
   * Upstream's autolink plugin is an `appendTransaction` hook that fires on
   * any doc-changing transaction, and y-tiptap mirrors every peer edit into
   * this editor as one. So a peer typing "https://… " made every other open
   * editor append a link mark and write it into the shared doc as its own
   * edit: duplicate traffic, and — since the server records who changed a
   * page — a contributor credit for someone who only had the page open. The
   * peer's own editor already linked the URL in its local transaction, so
   * skipping the mirrored copy loses nothing.
   *
   * The batch is skipped only when every transaction in it came from y-sync;
   * a local keystroke batched alongside a remote change still autolinks.
   * Undo/redo also arrives as a y-sync change (Yjs UndoManager owns undo),
   * which is fine: re-linking text the user just undid would be wrong anyway.
   *
   * The plugin is found by its key name because upstream does not export it;
   * autolink.test.ts fails if that lookup ever stops matching.
   */
  addProseMirrorPlugins() {
    return (this.parent?.() ?? []).map((plugin) => {
      const keyName = (plugin as unknown as { key?: string }).key ?? "";
      const append = plugin.spec.appendTransaction;
      if (!keyName.startsWith("autolink$") || !append) return plugin;
      return new Plugin({
        ...plugin.spec,
        appendTransaction: (transactions, oldState, newState) => {
          if (transactions.every(isRemoteSync)) return null;
          return append.call(plugin, transactions, oldState, newState);
        },
      });
    });
  },
});

/** True for a transaction y-tiptap dispatched to mirror a Y.Doc change that
 *  did not originate in this editor (a peer's edit, or a Yjs undo/redo). */
function isRemoteSync(tr: Transaction): boolean {
  const meta = tr.getMeta(ySyncPluginKey) as { isChangeOrigin?: boolean } | undefined;
  return meta?.isChangeOrigin === true;
}

/**
 * v3's Image adds `width` and `height`. Nothing in knot sets them — the
 * upload path calls setImage({ src }) and the paste sanitiser's attribute
 * allowlist drops both — but leaving them declared would put two attributes
 * in the document schema that `tools/schema.json` does not know and
 * `to_markdown` cannot emit. Dropping them keeps the stored schema identical
 * to what every existing document was written against, which is the property
 * worth protecting in a CRDT.
 *
 * Adopting image dimensions instead is a feature: declare them in
 * tools/schema.json and teach to_markdown to write them.
 */
const KnotImage = Image.extend({
  addAttributes() {
    const attrs: Record<string, unknown> = { ...(this.parent?.() ?? {}) };
    delete attrs.width;
    delete attrs.height;
    return attrs;
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
      // v2 called this `history`. Still disabled for the same reason: the Yjs
      // UndoManager owns undo, and a second history plugin fights it.
      undoRedo: false,
      codeBlock: false,
      // Disable the camelCase node defaults; we re-add snake_case versions
      // below so the Y.XmlFragment matches our canonical schema.
      bulletList: false,
      orderedList: false,
      listItem: false,
      hardBreak: false,
      horizontalRule: false,
      // Bundled by StarterKit from v3 on. Link and Underline are registered
      // explicitly below, and registering either twice makes the second
      // configure() lose the argument fight for the click handler.
      link: false,
      underline: false,
      // Appends a paragraph to any document not ending in one. Harmless in a
      // local editor; here it is a write to a shared CRDT that every peer
      // receives and that reaches the markdown export, triggered by opening
      // a document rather than by editing it.
      trailingNode: false,
      // Its list types default to camelCase, so it is inert against this
      // schema. Off rather than registered-and-doing-nothing.
      listKeymap: false,
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
    // Successor to CollaborationCursor, which has no v3 release. Only ever
    // reads `provider.awareness`, so the shim stands. `user.color` must be
    // 6-digit hex: the extension replaces anything else with "transparent",
    // which renders an invisible caret rather than warning (see colorFor).
    CollaborationCaret.configure({
      provider: { awareness: opts.awareness } as never,
      user: opts.user,
    }),
    KnotImage.configure({ inline: false, allowBase64: false }),
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
