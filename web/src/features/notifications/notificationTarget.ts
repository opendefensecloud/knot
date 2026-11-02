import type { Notification } from "../../lib/notifications.api";

/** Suffix after a bolded actor name, e.g. "<Alice> mentioned you". Only
 *  meaningful when the row has an actor — see `ACTORLESS_LABEL` for the
 *  rest. */
export const KIND_LABEL: Record<Notification["kind"], string> = {
  mention: "mentioned you",
  reply: "replied in your thread",
  task_assigned: "assigned you a task",
  task_due: "task is overdue",
  doc_shared: "shared a document with you",
  suggestion: "suggested a change",
  suggestion_decided: "reviewed your suggestion",
};

/** Full sentence for a notification with no actor. `task_due` is
 *  actor-less by design — it's system-generated, not caused by a person —
 *  and `task_assigned` can be too, on the live-editing path the design
 *  spec's §2 documents (a co-edit that assigns you a task has no single
 *  "who assigned this" answer). Rendering either as "knot task is
 *  overdue" makes the product sound like it's talking about itself; a
 *  `Record` over every kind keeps this exhaustive, so a sixth kind is a
 *  compile error here rather than a silent "knot" fallback. */
export const ACTORLESS_LABEL: Record<Notification["kind"], string> = {
  mention: "You were mentioned",
  reply: "New reply in your thread",
  task_assigned: "Task assigned to you",
  task_due: "Task is overdue",
  doc_shared: "Document shared with you",
  suggestion: "New suggestion on your page",
  suggestion_decided: "Your suggestion was reviewed",
};

/** How to render a notification row: either `{ actor, label }` for the
 *  bolded-name form ("<Alice> mentioned you") or `{ actor: null, label }`
 *  for a plain statement when the row carries no actor. */
export function notificationText(n: Notification): { actor: string | null; label: string } {
  return n.actor_display_name
    ? { actor: n.actor_display_name, label: KIND_LABEL[n.kind] }
    : { actor: null, label: ACTORLESS_LABEL[n.kind] };
}

/** Where a notification takes you. A suggestion-anchored one (including a
 *  reply or mention on a suggestion) appends ?suggestion=<id> so DocPage can
 *  open the Suggestions tab on that card; otherwise a comment-anchored one
 *  appends ?thread=<id> so DocPage can open the sidebar on that thread. */
export function targetPath(n: Notification): string | null {
  if (!n.doc_id) return null;
  const suggestion = typeof n.data.suggestion_id === "string" ? n.data.suggestion_id : null;
  if (suggestion) return `/doc/${n.doc_id}?suggestion=${suggestion}`;
  const thread = typeof n.data.thread_id === "string" ? n.data.thread_id : null;
  return thread ? `/doc/${n.doc_id}?thread=${thread}` : `/doc/${n.doc_id}`;
}
