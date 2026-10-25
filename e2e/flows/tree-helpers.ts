/**
 * Helpers for the sidebar tree specs (tree-reorder, tree-moves).
 *
 * Pages are seeded through the API and every result is read back from it,
 * so a spec asserts what the server stored, not what the sidebar shows for a
 * moment. Drags move the pointer the way a hand does: press on a row, a few
 * px to start the drag, then on to the target in steps, so every row on the
 * way sees the pointer pass. The helper lets go only once the target row
 * shows its drop zone (data-drop-zone, data-drop-depth). A build from before
 * those attributes has nothing to wait for: run it with E2E_NO_ZONE_WAIT=1,
 * and the helper waits two frames instead.
 */
import { expect, type APIRequestContext, type Page } from "@playwright/test";

/** Pixels per tree level: the rows' `paddingLeft: 4 + depth * 12`. */
export const INDENT = 12;

/** The part of a row a drop lands in: its top quarter, its middle (nest
 *  into it) or its bottom quarter. */
export type Zone = "above" | "into" | "below";

/** How far down a row's link the pointer goes for each zone. */
const ZONE_AT: Record<Zone, number> = { above: 0.1, into: 0.5, below: 0.9 };

/** Signs up the workspace owner through /setup and waits for the sidebar. */
export async function setupOwner(page: Page): Promise<void> {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await expect(page.getByTestId("doc-tree")).toBeVisible({ timeout: 10_000 });
}

async function writeHeaders(request: APIRequestContext): Promise<Record<string, string>> {
  const { cookies } = await request.storageState();
  const csrf = cookies.find((c) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found: sign in before seeding");
  return { "X-CSRF-Token": csrf.value };
}

/** A page to seed: its title, and its parent's title when it is a subpage. */
export type PageSpec<T extends string> = { title: T; parent?: T };

/** Creates the pages in order. Each goes right after the page seeded before
 *  it under the same parent (`after_id`), so the sidebar lists them in the
 *  order given. A parent must come before its subpages. Returns the ids by
 *  title, so titles must be unique. */
export async function seedPages<T extends string>(
  request: APIRequestContext,
  specs: PageSpec<T>[],
): Promise<Record<T, string>> {
  const headers = await writeHeaders(request);
  const ids = {} as Record<T, string>;
  const lastUnder = new Map<T | undefined, string>();
  for (const { title, parent } of specs) {
    const parentId = parent === undefined ? undefined : ids[parent];
    if (parent !== undefined && parentId === undefined) {
      throw new Error(`seed "${title}": its parent "${parent}" isn't seeded before it`);
    }
    const res = await request.post("/api/docs", {
      headers,
      data: { title, parent_id: parentId, after_id: lastUnder.get(parent) },
    });
    expect(res.status(), `create "${title}"`).toBe(201);
    const id = ((await res.json()) as { id: string }).id;
    ids[title] = id;
    lastUnder.set(parent, id);
  }
  return ids;
}

type ApiDoc = { id: string; parent_id: string | null; title: string; sort_key: string };

/** The tree as GET /api/docs has it, in sidebar order: one entry per page,
 *  its title path ("Parent/Child"). Equal outlines mean every page has the
 *  same parent_id and the same place among its siblings. */
export async function outline(request: APIRequestContext): Promise<string[]> {
  const res = await request.get("/api/docs");
  expect(res.status()).toBe(200);
  const docs = (await res.json()) as ApiDoc[];
  const listed = new Set(docs.map((d) => d.id));
  // Siblings by sort_key in code-unit order: the sidebar's, and the
  // server's COLLATE "C". The sort is stable, as the sidebar's is.
  const sorted = [...docs].sort((a, b) =>
    a.sort_key < b.sort_key ? -1 : a.sort_key > b.sort_key ? 1 : 0,
  );
  const lines: string[] = [];
  const walk = (parent: string | null, path: string) => {
    for (const d of sorted) {
      const p = d.parent_id !== null && listed.has(d.parent_id) ? d.parent_id : null;
      if (p !== parent) continue;
      lines.push(path + d.title);
      walk(d.id, `${path}${d.title}/`);
    }
  };
  walk(null, "");
  return lines;
}

/** Opens `path` and waits until the sidebar shows every page in `ids`. */
export async function showTree(page: Page, ids: string[], path = "/"): Promise<void> {
  await page.goto(path);
  for (const id of ids) {
    await expect(page.getByTestId(`doc-row-${id}`)).toBeVisible({ timeout: 10_000 });
  }
}

/** The move requests (POST /api/docs/<id>/move) the page sends from now
 *  on, by page id. */
export function watchMoves(page: Page): string[] {
  const sent: string[] = [];
  page.on("request", (r) => {
    const m = /^\/api\/docs\/([^/]+)\/move$/.exec(new URL(r.url()).pathname);
    if (m && r.method() === "POST") sent.push(m[1]!);
  });
  return sent;
}

/** A request from the page to the server and back. A drop sends its move
 *  while handling the release, so once this returns, any move it sent has
 *  been seen by `watchMoves`. */
export async function roundTrip(page: Page): Promise<void> {
  await page.evaluate(async () => {
    await fetch("/api/docs");
  });
}

async function linkBox(page: Page, id: string) {
  const box = await page.getByTestId(`doc-row-${id}`).boundingBox();
  if (!box) throw new Error(`doc-row-${id} is not on screen`);
  return box;
}

/** Waits `n` animation frames, so the page has handled the last pointer
 *  move: React renders a mousemove's updates in a task of their own. */
async function frames(page: Page, n: number): Promise<void> {
  await page.evaluate(
    (count) =>
      new Promise<void>((resolve) => {
        const tick = (left: number) =>
          left === 0 ? resolve() : requestAnimationFrame(() => tick(left - 1));
        tick(count);
      }),
    n,
  );
}

export type DragOptions = {
  /** The part of the target row to let go over. Not used for a point target,
   *  except for the zone to wait for. */
  zone?: Zone;
  /** The horizontal drag in px from where the drag started (INDENT per
   *  level). Not used for a point target. */
  dx?: number;
  /** Wait for the drop zone to show before letting go. A drop that changes
   *  nothing (or goes outside the tree) shows none: pass false there. */
  waitForZone?: boolean;
  /** Runs once the drag has started, before the pointer heads for the target. */
  whileDragging?: () => Promise<void>;
};

/** Drags page `fromId` by its row and drops it on page `to.id`'s row, in
 *  `zone`, `dx` px left or right of where it started; or at the point
 *  `{ x, y }`. */
export async function dragRow(
  page: Page,
  fromId: string,
  to: { id: string } | { x: number; y: number },
  {
    zone = "into",
    dx = 0,
    waitForZone = !process.env.E2E_NO_ZONE_WAIT,
    whileDragging,
  }: DragOptions = {},
): Promise<void> {
  // 1. Press on the row's link: every version of the sidebar has it, and
  //    its mousedown bubbles to the row's drag listeners.
  const from = await linkBox(page, fromId);
  const start = { x: from.x + from.width / 2, y: from.y + from.height / 2 };
  // dnd-kit's live region still holds the end of the last drag, if any.
  const live = page.locator("[id^='DndLiveRegion']");
  const said = (await live.textContent()) ?? "";
  await page.mouse.move(start.x, start.y);
  await page.mouse.down();

  // 2. 10px down starts the drag (past the sensors' 6px). The pointer stays
  //    on the row's own lower half, where a drop changes nothing. The live
  //    region's first news ("Picked up …", soon replaced by "… over …")
  //    comes in the render that drops the page's subpages from the tree.
  await page.mouse.move(start.x, start.y + 10);
  await expect(live).not.toHaveText(said);
  await whileDragging?.();

  // 3. Measured now, not before the drag: the dragged page's subpages have
  //    left the tree, and the rows below them have moved up.
  let end: { x: number; y: number };
  if ("id" in to) {
    const target = await linkBox(page, to.id);
    end = { x: start.x + dx, y: target.y + target.height * ZONE_AT[zone] };
  } else {
    end = to;
  }

  // 4. On to the target in steps, as a hand moves; never a jump.
  await page.mouse.move(end.x, end.y, { steps: 10 });

  // 5. Let go only once the page shows where the drop goes.
  if (waitForZone) {
    const row =
      "id" in to
        ? page.getByTestId(`tree-row-${to.id}`)
        : page.locator(`[data-testid^='tree-row-'][data-drop-zone]`);
    await expect(row).toHaveAttribute("data-drop-zone", zone);
    await expect(row).toHaveAttribute("data-drop-depth", /^\d+$/);
  } else {
    await frames(page, 2);
  }

  // 6. Let go.
  await page.mouse.up();
}
