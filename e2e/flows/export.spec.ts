// Page export to Word and OpenDocument: the header menu renders the Mermaid
// diagram in the browser, posts it with the export request, and saves the
// server's file. The download is read back as a zip to check the document
// body, the heading style and the diagram's picture part.
import { readFileSync } from "node:fs";

import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";
import { zipEntries } from "../support/zip";

const EMAIL = "o@e.com";
const PASSWORD = "owner-hunter22";
const TITLE = "Export test";

const MARKDOWN = `# Export test

- [x] done
- [ ] todo

| A | B |
|---|---|
| 1 | 2 |

\`\`\`mermaid
graph TD; A-->B
\`\`\`
`;

test.beforeAll(async () => {
  reset();
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, display_name: "O", password: PASSWORD }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

async function writeHeaders(page: Page) {
  const csrf = (await page.context().cookies()).find((c) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return { "X-CSRF-Token": csrf.value };
}

/** Creates the page and fills it; the Markdown import doesn't set the title. */
async function seedPage(page: Page): Promise<string> {
  const headers = await writeHeaders(page);
  const created = await page.request.post("/api/docs", { headers, data: { title: TITLE } });
  expect(created.status()).toBe(201);
  const id = (await created.json()).id as string;
  const imported = await page.request.post(`/api/docs/${id}/markdown?mode=replace`, {
    headers: { ...headers, "Content-Type": "text/markdown" },
    data: MARKDOWN,
  });
  expect(imported.status()).toBe(204);
  return id;
}

/** Signs in, seeds the page, exports it from the header menu and unzips the download. */
async function exportPage(page: Page, item: "export-docx" | "export-odt") {
  await signIn(page);
  const id = await seedPage(page);
  await page.goto(`/doc/${id}`);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  await page.getByTestId("doc-export").click();
  const downloaded = page.waitForEvent("download");
  await page.getByTestId(item).click();
  const download = await downloaded;
  const path = await download.path();
  return { name: download.suggestedFilename(), entries: zipEntries(readFileSync(path)) };
}

test("export: a page downloads as a Word document with its diagram", async ({ page }) => {
  const { name, entries } = await exportPage(page, "export-docx");

  expect(name).toBe("Export test.docx");
  const body = entries.get("word/document.xml")?.toString("utf8");
  expect(body).toBeDefined();
  expect(body).toContain(TITLE);
  expect(body).toContain('w:val="Heading1"');
  expect([...entries.keys()]).toContain("word/media/image1.png");
});

test("export: a page downloads as an OpenDocument text with its diagram", async ({ page }) => {
  const { name, entries } = await exportPage(page, "export-odt");

  expect(name).toBe("Export test.odt");
  const body = entries.get("content.xml")?.toString("utf8");
  expect(body).toBeDefined();
  expect(body).toContain(TITLE);
  expect(body).toContain('text:outline-level="1"');
  expect([...entries.keys()]).toContainEqual(expect.stringMatching(/^Pictures\//));
});
