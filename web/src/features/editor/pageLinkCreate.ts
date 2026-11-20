/**
 * Creating a subpage from the `[[` picker. The page goes under the current
 * one; a refusal or a network failure is said in a toast and resolves null,
 * so the picker leaves the typed text alone. The sidebar's doc list is
 * refreshed without waiting for it, so the link appears at once.
 */

import type { ApiResult } from "../../lib/api";
import { notifyFailure, type Notify } from "../../lib/limitErrors";
import type { DocCreate } from "../docs/docs.api";

const FAILED = "Couldn't create the page.";

export type SubpageCreatorDeps = {
  create: (body: DocCreate) => Promise<ApiResult<unknown>>;
  refreshDocs: () => void;
  notify: Notify;
};

export function subpageCreator(
  parentId: string,
  deps: SubpageCreatorDeps,
): (title: string) => Promise<{ id: string; title: string } | null> {
  return async (title) => {
    let r: ApiResult<unknown>;
    try {
      r = await deps.create({ title, parent_id: parentId });
    } catch {
      notifyFailure(deps.notify, null, FAILED);
      return null;
    }
    if ("error" in r) {
      notifyFailure(deps.notify, r.error, FAILED);
      return null;
    }
    deps.refreshDocs();
    const created = r.ok as { id: string; title: string };
    return { id: created.id, title: created.title };
  };
}
