import { expect, test } from "@playwright/test";

import { docText } from "../support/docText";
import { reset } from "../support/reset";

test.beforeAll(reset);

test("two users editing concurrently converge on both screens", async ({ browser }) => {
  // Alice sets up the workspace + creates a doc + invites Bob with password.
  const aliceCtx = await browser.newContext();
  const alice = await aliceCtx.newPage();
  await alice.goto("/setup");
  await alice.getByTestId("setup-email").fill("alice@example.com");
  await alice.getByTestId("setup-display-name").fill("Alice");
  await alice.getByTestId("setup-password").fill("alice-hunter22");
  await alice.getByTestId("setup-submit").click();
  await alice.getByTestId("new-doc").click();
  await alice.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await alice.getByTestId("new-doc-blank").click();
  await alice.waitForURL(/\/doc\/.+/);
  const docUrl = alice.url();

  await alice.goto("/members");
  await alice.getByTestId("invite-email").fill("bob@example.com");
  await alice.getByTestId("invite-role").selectOption("editor");
  await alice.getByTestId("invite-password").fill("bob-hunter22");
  await alice.getByTestId("invite-submit").click();
  // Wait for Bob to appear in the members table (any member-* testid besides Alice's).
  await expect(alice.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });

  // Bob signs in in a separate browser context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@example.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/(?:doc\/.+)?$/, { timeout: 5_000 });

  // Both navigate to the doc.
  await alice.goto(docUrl);
  await bob.goto(docUrl);

  // Both reach connected.
  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await expect(bob.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // Type from each side.
  const aliceEditor = alice.locator("[data-testid='editor-host'] .ProseMirror");
  const bobEditor = bob.locator("[data-testid='editor-host'] .ProseMirror");

  await aliceEditor.click();
  await alice.keyboard.type("Hello from Alice. ");
  // Wait for Alice's text to propagate to Bob's editor before Bob starts typing,
  // so their edits land at different positions and the CRDT merge is unambiguous.
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 8_000 }).toMatch(/Hello from Alice\./);

  // Click the editor, move cursor to end of line, then type Bob's contribution.
  // The 200ms pause after End lets ProseMirror settle its cursor state.
  await bobEditor.click();
  await bob.keyboard.press("End");
  await bob.waitForTimeout(200);
  await bob.keyboard.type("And from Bob.");

  // Both screens see both contributions within the poll window.
  // Poll Bob's editor first so we know his Yjs doc has the text before checking Alice.
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => bobEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/And from Bob\./);
  await expect.poll(() => aliceEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => aliceEditor.evaluate(docText), { timeout: 5_000 }).toMatch(/And from Bob\./);

  // Alice can actually SEE Bob. Nothing else in the suite asserts that a
  // remote caret renders — docText above deliberately strips the label, so a
  // caret that silently stopped being drawn would leave every assertion in
  // this file green. The colour matters too: the awareness payload is
  // validated against /^#[0-9a-fA-F]{6}$/, and a value outside that either
  // warns and renders (today) or is replaced with `transparent`.
  const bobCaret = aliceEditor.locator(".collaboration-cursor__caret");
  await expect(bobCaret).toHaveCount(1, { timeout: 8_000 });
  // Bob was invited by email with no display name, so the server derives
  // one from the local part.
  await expect(bobCaret.locator(".collaboration-cursor__label")).toHaveText("bob");

  const caretColor = await bobCaret.evaluate((el) => getComputedStyle(el).borderLeftColor);
  expect(caretColor, "the remote caret has no colour — it is invisible").not.toBe("transparent");
  expect(caretColor).not.toBe("rgba(0, 0, 0, 0)");
  expect(caretColor, `unexpected caret colour ${caretColor}`).toMatch(/^rgba?\(/);

  await aliceCtx.close();
  await bobCtx.close();
});
