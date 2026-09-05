import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);

test("viewer doesn't see invite/remove + new-doc", async ({ browser }) => {
  // Owner sets up workspace + invites Bob as viewer.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("o@e.com");
  await owner.getByTestId("setup-display-name").fill("O");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);

  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("bob@e.com");
  await owner.getByTestId("invite-role").selectOption("viewer");
  await owner.getByTestId("invite-password").fill("bob-hunter22");
  await owner.getByTestId("invite-submit").click();
  await expect(owner.locator("[data-testid^='member-']")).toHaveCount(2, {
    timeout: 5_000,
  });

  // Bob signs in.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@e.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();

  await bob.waitForURL(/\/(?:doc\/.+)?$/);
  // No "+ New" button for viewer
  await expect(bob.getByTestId("new-doc")).toHaveCount(0);

  await bob.goto("/members");
  // No invite form for viewer
  await expect(bob.getByTestId("invite-form")).toHaveCount(0);

  await ownerCtx.close();
  await bobCtx.close();
});
