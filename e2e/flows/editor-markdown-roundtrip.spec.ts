import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

/**
 * Every canonical node and mark, authored in the editor, exported as Markdown.
 *
 * Only three node kinds ever travel editor → Y.Doc → `to_markdown` in the rest
 * of the suite: paragraph, image and excalidraw_board. Everything else is
 * either exercised in the opposite direction (markdown import) or not at all.
 *
 * That gap matters because the two sides name things independently. knot's
 * editor renames Tiptap's camelCase nodes to the snake_case schema the Rust
 * serializer expects, and a drift is punished asymmetrically: an unknown NODE
 * makes the export 500, but an unknown MARK hits `to_markdown`'s catch-all arm
 * and is dropped without a trace. Formatting simply disappears from the export,
 * the share page and the search index.
 *
 * The document is authored by pasting Markdown, which routes through the
 * editor's own paste handler and the ProseMirror schema, so the nodes really
 * are built by the editor rather than by the server's parser.
 */

test.beforeAll(reset);

const EMAIL = "owner@roundtrip.test";
const PASSWORD = "hunter22!roundtrip";

test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

const SOURCE = [
  "# Heading one",
  "",
  "## Heading two",
  "",
  "A paragraph with **bold**, *italic*, `code`, ~~strike~~, <u>underline</u>,",
  'and a [link](https://example.test "Link title").',
  "",
  "> A quotation.",
  "",
  "- bullet one",
  "- bullet two",
  "",
  "1. ordered one",
  "2. ordered two",
  "",
  "- [ ] open task",
  "- [x] done task",
  "",
  "```js",
  "const x = 1;",
  "```",
  "",
  "---",
  "",
  "| Col A | Col B |",
  "| --- | --- |",
  "| a1 | b1 |",
  "",
].join("\n");

async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

async function newDoc(page: Page): Promise<string> {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 15_000,
  });
  return page.url().match(/\/doc\/([^/?#]+)/)?.[1] ?? "";
}

test("every canonical node and mark survives editor → Markdown", async ({ page }) => {
  await signIn(page);
  const docId = await newDoc(page);

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.evaluate((src: string) => {
    const dt = new DataTransfer();
    dt.setData("text/plain", src);
    const pm = document.querySelector("[data-testid='editor-host'] .ProseMirror")!;
    pm.dispatchEvent(
      new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: dt }),
    );
  }, SOURCE);

  // The paste has to land in the editor before the export can see it.
  await expect(editor.locator("h1")).toHaveText("Heading one", { timeout: 10_000 });
  await expect(editor.locator("table")).toHaveCount(1);

  const exported = await page.evaluate(async (id: string) => {
    const r = await fetch(`/api/docs/${id}/markdown`, { credentials: "include" });
    if (!r.ok) return `EXPORT FAILED ${r.status}: ${await r.text()}`;
    return r.text();
  }, docId);

  expect(exported, "the export failed outright — an unknown NODE name does this")
    .not.toMatch(/^EXPORT FAILED/);

  // Named one construct at a time so a failure says which one vanished.
  // Whitespace is matched loosely on purpose: this guards node and mark
  // NAMES, not the serializer's exact spacing.
  const expected: [string, RegExp][] = [
    ["heading level 1", /^# Heading one$/m],
    ["heading level 2", /^## Heading two$/m],
    ["bold", /\*\*bold\*\*/],
    ["italic", /\*italic\*/],
    ["inline code", /`code`/],
    ["strike", /~~strike~~/],
    ["underline", /<u>underline<\/u>/],
    ["link with title", /\[link\]\(https:\/\/example\.test "Link title"\)/],
    ["blockquote", /^> A quotation\.$/m],
    ["bullet list", /^- bullet one$/m],
    ["ordered list", /^1\. ordered one$/m],
    ["unchecked task", /^- \[ \]\s+open task$/m],
    ["checked task", /^- \[x\]\s+done task$/m],
    ["fenced code block with language", /^```js$/m],
    ["code block content", /^const x = 1;$/m],
    ["horizontal rule", /^---$/m],
    ["table header row", /^\| Col A \| Col B \|$/m],
    ["table body row", /^\| a1 \| b1 \|$/m],
  ];

  for (const [name, pattern] of expected) {
    expect(exported, `${name} did not survive the round trip`).toMatch(pattern);
  }
});
