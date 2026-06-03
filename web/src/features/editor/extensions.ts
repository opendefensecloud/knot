import Collaboration from "@tiptap/extension-collaboration";
import CollaborationCursor from "@tiptap/extension-collaboration-cursor";
import Image from "@tiptap/extension-image";
import Link from "@tiptap/extension-link";
import StarterKit from "@tiptap/starter-kit";
import type { Awareness } from "y-protocols/awareness";
import type * as Y from "yjs";

import { Attachment } from "./nodes/AttachmentNode";
import { MermaidCodeBlock } from "./nodes/MermaidCodeBlock";

/** Canonical Tiptap extension set that matches the server schema generated
 *  from `tools/schema.json`. History is disabled because Yjs UndoManager
 *  owns undo. */
export function createExtensions(opts: {
  doc: Y.Doc;
  awareness: Awareness;
  user: { name: string; color: string };
}) {
  return [
    StarterKit.configure({
      history: false,
      codeBlock: false,
    }),
    MermaidCodeBlock,
    Link.configure({
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
  ];
}
