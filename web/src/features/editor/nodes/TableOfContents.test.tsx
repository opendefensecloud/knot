/**
 * The table of contents box: a live list of the page's headings, built on
 * render and never written to the document; the depth is its only setting.
 */

import { act, cleanup, render, screen, fireEvent } from "@testing-library/react";
import { EditorContent, useEditor, type Editor } from "@tiptap/react";
import { useEffect } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { createExtensions } from "../extensions";

let editorRef: Editor | null = null;

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

const entries = () => screen.queryAllByTestId("toc-entry").map((a) => a.textContent);

afterEach(() => {
  cleanup();
  editorRef = null;
});

const PAGE =
  '<div data-knot-toc="true" data-max-level="3"></div><h1>Plan</h1><h2>Tasks</h2><h3>Details</h3><h4>Deep</h4>';

describe("table of contents box", () => {
  it("lists headings in a box with links", async () => {
    mount(PAGE);
    await screen.findByTestId("toc-box");
    expect(entries()).toEqual(["Plan", "Tasks", "Details"]);
    expect(screen.getAllByTestId("toc-entry").map((a) => a.getAttribute("href"))).toEqual([
      "#plan",
      "#tasks",
      "#details",
    ]);
    expect(screen.getByTestId("toc-box")).toHaveTextContent("Contents");
  });

  it("updates live and never writes to the doc", async () => {
    const ydoc = mount(PAGE);
    await screen.findByTestId("toc-box");
    const origins: unknown[] = [];
    ydoc.on("update", (_u: Uint8Array, origin: unknown) => origins.push(origin));

    // A collaborator renames "Tasks".
    const remote = new Y.Doc();
    Y.applyUpdate(remote, Y.encodeStateAsUpdate(ydoc));
    const frag = remote.getXmlFragment("default");
    const h2 = frag.toArray().find((n) => (n as Y.XmlElement).nodeName === "heading" && Number((n as Y.XmlElement).getAttribute("level")) === 2) as Y.XmlElement;
    const text = h2.get(0) as Y.XmlText;
    remote.transact(() => {
      text.delete(0, text.length);
      text.insert(0, "Aufgaben");
    });
    act(() => {
      Y.applyUpdate(ydoc, Y.encodeStateAsUpdate(remote, Y.encodeStateVector(ydoc)), "remote");
    });

    expect(entries()).toEqual(["Plan", "Aufgaben", "Details"]);
    expect(origins.every((o) => o === "remote")).toBe(true);
  });

  it("changes depth via Levels", async () => {
    mount(PAGE);
    await screen.findByTestId("toc-box");
    act(() => {
      fireEvent.change(screen.getByTestId("toc-levels"), { target: { value: "1" } });
    });
    let toc: { attrs: Record<string, unknown> } | null = null;
    editorRef!.state.doc.descendants((n) => {
      if (n.type.name === "table_of_contents") toc = n;
    });
    expect(Number(toc!.attrs.maxLevel)).toBe(1);
    expect(entries()).toEqual(["Plan"]);
  });

  it("hides Levels when not editable", async () => {
    mount(PAGE, false);
    await screen.findByTestId("toc-box");
    expect(screen.queryByTestId("toc-levels")).toBeNull();
  });

  it("shows No headings yet", async () => {
    mount('<div data-knot-toc="true"></div><p>text</p>');
    await screen.findByTestId("toc-box");
    expect(screen.getByTestId("toc-box")).toHaveTextContent("No headings yet");
  });
});
