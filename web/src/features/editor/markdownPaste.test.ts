import { describe, expect, it } from "vitest";

import { looksLikeMarkdown, markdownToHtml } from "./markdownPaste";

describe("looksLikeMarkdown — converts", () => {
  it("a subheading, the issue's own example", () => {
    expect(looksLikeMarkdown("## Heading\n\nSome text.")).toBe(true);
  });

  it("a fenced code block on its own", () => {
    expect(looksLikeMarkdown("```js\nconst a = 1;\n```")).toBe(true);
  });

  it("a GFM table on its own", () => {
    expect(looksLikeMarkdown("| a | b |\n| --- | --- |\n| 1 | 2 |")).toBe(true);
  });

  it("a bullet list", () => {
    expect(looksLikeMarkdown("- one\n- two\n- three")).toBe(true);
  });

  it("an ordered list", () => {
    expect(looksLikeMarkdown("1. one\n2. two")).toBe(true);
  });

  it("prose carrying two different inline cues", () => {
    expect(looksLikeMarkdown("See [the docs](https://x.test) for **details**.")).toBe(true);
  });

  it("a heading plus a list", () => {
    expect(looksLikeMarkdown("# Release notes\n\n- fixed a thing")).toBe(true);
  });

  it("a task list", () => {
    expect(looksLikeMarkdown("- [ ] todo\n- [x] done")).toBe(true);
  });
});

describe("looksLikeMarkdown — leaves alone", () => {
  it("one line of prose", () => {
    expect(looksLikeMarkdown("Just a sentence I copied from somewhere.")).toBe(false);
  });

  it("prose mentioning C# and F#", () => {
    expect(looksLikeMarkdown("C# is fine and so is F#.")).toBe(false);
  });

  it("a Python snippet whose comments start with #", () => {
    const py = [
      "# fetch the rows",
      "rows = db.query(SQL)",
      "# drop the empty ones",
      "rows = [r for r in rows if r.ok]",
    ].join("\n");
    expect(looksLikeMarkdown(py)).toBe(false);
  });

  it("a shell script", () => {
    expect(looksLikeMarkdown("#!/bin/bash\n# build it\nset -e\nmake all")).toBe(false);
  });

  it("a config file with blank-separated # comments", () => {
    const toml = '# server\n\nport = 8080\n\n# database\n\nurl = "postgres://"';
    expect(looksLikeMarkdown(toml)).toBe(false);
  });

  it("empty text", () => {
    expect(looksLikeMarkdown("")).toBe(false);
  });

  it("a lone hyphen", () => {
    expect(looksLikeMarkdown("-")).toBe(false);
  });
});

describe("markdownToHtml", () => {
  it("renders headings and lists", () => {
    const html = markdownToHtml("## Title\n\n- one\n- two\n");
    expect(html).toContain("<h2>Title</h2>");
    expect(html).toContain("<li>one</li>");
  });

  it("promotes task items to data-checked and drops the input", () => {
    const html = markdownToHtml("- [ ] todo\n- [x] done\n");
    expect(html).toContain('data-checked="false"');
    expect(html).toContain('data-checked="true"');
    expect(html).not.toContain("<input");
  });

  it("keeps the code fence language as a class", () => {
    expect(markdownToHtml("```js\nconst a = 1;\n```")).toContain("language-js");
  });

  it("renders a GFM table", () => {
    const html = markdownToHtml("| a | b |\n| --- | --- |\n| 1 | 2 |");
    expect(html).toContain("<table>");
    expect(html).toContain("<td>1</td>");
  });

  it("strips embedded scripts and handlers", () => {
    const html = markdownToHtml("# Hi\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>");
    expect(html).not.toContain("script");
    expect(html).not.toContain("onerror");
  });
});
