/**
 * The callout box in the editor: a blockquote with a kind shows a header
 * with the kind's icon, a title field and (when editable) a kind select; a
 * plain quote stays a plain quote. A knot that does not know the
 * attributes keeps the box's text.
 */

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Editor } from "@tiptap/core";
import Collaboration from "@tiptap/extension-collaboration";
import { EditorContent, useEditor, type Editor as ReactEditor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { useEffect } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { createExtensions } from "../extensions";

let editorRef: ReactEditor | null = null;

function Harness({ ydoc, editable }: { ydoc: Y.Doc; editable: boolean }) {
  const editor = useEditor({
    editable,
    extensions: createExtensions({
      doc: ydoc,
      awareness: new Awareness(ydoc),
      user: { name: "T", color: "#336699" },
    }),
  });
  useEffect(() => {
    editorRef = editor;
  }, [editor]);
  return <EditorContent editor={editor} />;
}

function mount(html: string, editable = true) {
  const ydoc = new Y.Doc();
  render(<Harness ydoc={ydoc} editable={editable} />);
  act(() => {
    editorRef!.commands.setContent(html);
  });
  return ydoc;
}

/** The attributes of the first blockquote in the document. */
function quoteAttrs(): Record<string, unknown> {
  let attrs: Record<string, unknown> = {};
  let found = false;
  editorRef!.state.doc.descendants((n) => {
    if (!found && n.type.name === "blockquote") {
      attrs = n.attrs;
      found = true;
    }
  });
  return attrs;
}

afterEach(() => {
  cleanup();
  editorRef = null;
});

const WARNING = '<blockquote data-kind="warning"><p>Body</p></blockquote>';

describe("callout box", () => {
  it("renders a warning box with label placeholder and icon", async () => {
    mount(WARNING);
    const box = await screen.findByTestId("callout");
    expect(box).toHaveAttribute("data-kind", "warning");
    expect(screen.getByTestId("callout-title")).toHaveAttribute("placeholder", "Warning");
    expect(box.querySelector("svg")).not.toBeNull();
    expect(box).toHaveTextContent("Body");
  });

  it("typing a title stores it on the node, and undo brings the old one back", async () => {
    mount(WARNING);
    const input = await screen.findByTestId("callout-title");
    act(() => {
      // An <input> holds one line; the newline case is in callouts.test.ts.
      fireEvent.change(input, { target: { value: "  Wartung   am Freitag " } });
    });
    expect(quoteAttrs().title).toBe("Wartung am Freitag");
    act(() => {
      editorRef!.commands.undo();
    });
    expect(quoteAttrs().title ?? null).toBeNull();
  });

  it("the kind select changes the kind", async () => {
    mount(WARNING);
    await screen.findByTestId("callout");
    act(() => {
      fireEvent.change(screen.getByTestId("callout-kind"), { target: { value: "caution" } });
    });
    expect(quoteAttrs().kind).toBe("caution");
    await waitFor(() => expect(screen.getByTestId("callout")).toHaveAttribute("data-kind", "caution"));
  });

  it("has no select and no title field when not editable, but shows the title", async () => {
    mount('<blockquote data-kind="tip" data-title="Hinweis"><p>x</p></blockquote>', false);
    const box = await screen.findByTestId("callout");
    expect(screen.queryByTestId("callout-kind")).toBeNull();
    expect(box.querySelector("input")).toBeNull();
    expect(box).toHaveTextContent("Hinweis");
  });

  it("a plain quote renders unchanged", async () => {
    mount("<blockquote><p>q</p></blockquote>");
    await screen.findByText("q");
    expect(screen.queryByTestId("callout")).toBeNull();
    expect(editorRef!.view.dom.querySelector("blockquote")).not.toBeNull();
  });

  it("an unknown kind renders as a plain quote", async () => {
    mount("<blockquote><p>q</p></blockquote>");
    await screen.findByText("q");
    act(() => {
      editorRef!.commands.command(({ tr, state }) => {
        let pos = -1;
        state.doc.descendants((n, p) => {
          if (pos < 0 && n.type.name === "blockquote") pos = p;
        });
        tr.setNodeAttribute(pos, "kind", "danger");
        return true;
      });
    });
    expect(screen.queryByTestId("callout")).toBeNull();
    expect(screen.getByText("q")).toBeInTheDocument();
  });

  it("copy and paste as HTML keep the kind and the title", () => {
    mount('<blockquote data-kind="note" data-title="T"><p>x</p></blockquote>');
    const html = editorRef!.getHTML();
    act(() => {
      editorRef!.commands.setContent(html);
    });
    expect(quoteAttrs()).toMatchObject({ kind: "note", title: "T" });
  });
});

describe("a knot without callouts", () => {
  it("keeps the text of a box when it opens and edits it", async () => {
    // The box, written by this knot.
    const ydoc = mount('<blockquote data-kind="warning" data-title="T"><p>Keep me</p></blockquote>');
    await screen.findByTestId("callout");
    // An older knot: the schema's blockquote has no kind or title.
    const old = new Y.Doc();
    Y.applyUpdate(old, Y.encodeStateAsUpdate(ydoc));
    const oldEditor = new Editor({
      extensions: [StarterKit.configure({ undoRedo: false }), Collaboration.configure({ document: old })],
    });
    expect(oldEditor.getText()).toContain("Keep me");
    // It edits inside the box: text typed at the end of "Keep me".
    let end = -1;
    oldEditor.state.doc.descendants((n, pos) => {
      if (n.isText && n.text === "Keep me") end = pos + n.nodeSize;
    });
    oldEditor.chain().setTextSelection(end).insertContent(" and more").run();
    Y.applyUpdate(ydoc, Y.encodeStateAsUpdate(old, Y.encodeStateVector(ydoc)));
    const frag = ydoc.getXmlFragment("default");
    const quote = frag.toArray().find((n) => (n as Y.XmlElement).nodeName === "blockquote") as Y.XmlElement;
    expect(quote).toBeDefined();
    expect(quote.toJSON()).toContain("Keep me and more");
    oldEditor.destroy();
  });
});
