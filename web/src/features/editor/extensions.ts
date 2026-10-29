import Collaboration from "@tiptap/extension-collaboration";
import CollaborationCaret from "@tiptap/extension-collaboration-caret";
import Image from "@tiptap/extension-image";
import Link from "@tiptap/extension-link";
import Underline from "@tiptap/extension-underline";
import { Fragment, Slice, type Node as ProseMirrorNode } from "@tiptap/pm/model";
import { Plugin, PluginKey, Selection, type Transaction } from "@tiptap/pm/state";
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
   * A link ends where its text ends: text typed or pasted at either edge of
   * a link stays plain.
   *
   * Upstream ties this to the `autolink` option (`inclusive() { return
   * this.options.autolink }`), a v2 leftover from when autolink rewrote a
   * growing link's href. It no longer does, so with autolink on, every
   * keystroke at a link's end joined its text while the href stayed put.
   * Paste made that the common case: the paste rule keeps the caret inside a
   * link that ends the pasted text, precisely to honour `inclusive`
   * (tiptap#7692), so "paste a URL, keep typing" linked the rest of the
   * sentence. The same happened after a doc link, an edited date, an HTML
   * paste or a click at the end of any link, and typing at the start of a
   * paragraph joined a link that opened it.
   *
   * Tiptap's own fix (tiptap#8335, 3.31.4) only stops a typed space from
   * joining; punctuation, and text typed without a space, still did.
   * Non-inclusive is what prosemirror-schema-basic's link does, at the same
   * cost: retyping a link's last character, or typing over all of its text,
   * gives plain text. It also makes upstream's `exitable` ArrowRight handler
   * inert, which is wanted — at the end of a paragraph it wrote a stray space
   * into the shared doc.
   *
   * `inclusive` is a local editing rule and is never serialised, so the
   * Y.Doc, `tools/schema.json` and the server are unaffected. Leave
   * `excludes` alone: y-tiptap derives the `link` attribute key from it, and
   * `to_markdown` reads that key.
   */
  inclusive() {
    return false;
  },

  /**
   * Adjusts two upstream plugins; see localAutolinkOnly and
   * caretAfterPastedLink. Both are found by their key names because upstream
   * exports neither; autolink.test.ts and linkBoundary.test.ts fail if a
   * lookup ever stops matching. Adds pastedLinksWithoutUnderline.
   */
  addProseMirrorPlugins() {
    const upstream = (this.parent?.() ?? []).map((plugin) => {
      const keyName = (plugin as unknown as { key?: string }).key ?? "";
      if (keyName.startsWith("autolink$")) return localAutolinkOnly(plugin);
      if (keyName.startsWith("handlePasteLink$")) return caretAfterPastedLink(plugin);
      return plugin;
    });
    return [...upstream, pastedLinksWithoutUnderline()];
  },
});

/**
 * Pasted links lose the underline they were copied with.
 *
 * A link copied from a web page, Google Docs or Word carries the source's
 * link styling — `text-decoration: underline` on the anchor or on a span in
 * it — and Underline's parser turns that into an `underline` mark beside
 * `link`. Underline is inclusive and the link is not (see KnotLink), so at
 * the link's end the underline carried on into everything typed next: text
 * that was not part of the link, yet looked like it.
 *
 * Knot does not underline links, so an underline arriving with one is the
 * source page's look, not the author's. Underline on text outside a link is
 * kept, and so is content copied or dragged within knot (or from another
 * ProseMirror editor, which marks its clipboard HTML with `data-pm-slice`,
 * the same test Tiptap's paste rules use): there an underlined link was
 * underlined on purpose, with Mod-U. External drops go through the same
 * clipboard parser and are handled like pastes.
 *
 * Not covered: an underlined URL pasted without an anchor, which the link
 * paste rule only links after this has run.
 */
function pastedLinksWithoutUnderline(): Plugin {
  // transformPastedHTML and transformPasted run in one parseFromClipboard
  // call, so the flag never outlives the paste it describes.
  let fromProseMirror = false;
  return new Plugin({
    key: new PluginKey("knotPastedLinkUnderline"),
    props: {
      transformPastedHTML(html) {
        fromProseMirror = html.includes("data-pm-slice");
        return html;
      },
      transformPasted(slice, view) {
        const internal = fromProseMirror || view.dragging !== null;
        fromProseMirror = false;
        const { link, underline } = view.state.schema.marks;
        if (internal || !link || !underline) return slice;
        const strip = (fragment: Fragment): Fragment => {
          const nodes: ProseMirrorNode[] = [];
          fragment.forEach((node) => {
            let next = node.isText ? node : node.copy(strip(node.content));
            if (link.isInSet(next.marks)) next = next.mark(underline.removeFromSet(next.marks));
            nodes.push(next);
          });
          return Fragment.fromArray(nodes);
        };
        return new Slice(strip(slice.content), slice.openStart, slice.openEnd);
      },
    },
  });
}

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
 */
function localAutolinkOnly(plugin: Plugin): Plugin {
  const append = plugin.spec.appendTransaction;
  if (!append) return plugin;
  return new Plugin({
    ...plugin.spec,
    appendTransaction: (transactions, oldState, newState) => {
      if (transactions.every(isRemoteSync)) return null;
      return append.call(plugin, transactions, oldState, newState);
    },
  });
}

/**
 * Pasting a URL over selected text links that text and, upstream, leaves it
 * selected — so the next keystroke replaced the new link outright. The caret
 * moves to the link's end instead, where (non-inclusive) typing carries on
 * as plain text.
 *
 * `Selection.near` rather than a TextSelection at `to`: after select-all or a
 * table-cell selection, `to` is not inside a textblock, and a caret there
 * made the next keystroke open a new paragraph.
 */
function caretAfterPastedLink(plugin: Plugin): Plugin {
  const handlePaste = plugin.spec.props?.handlePaste;
  if (!handlePaste) return plugin;
  return new Plugin({
    ...plugin.spec,
    props: {
      ...plugin.spec.props,
      handlePaste: (view, event, slice) => {
        if (!handlePaste.call(plugin, view, event, slice)) return false;
        const { state } = view;
        view.dispatch(state.tr.setSelection(Selection.near(state.doc.resolve(state.selection.to), -1)));
        return true;
      },
    },
  });
}

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
