/**
 * Plain sentences for the page audit log (/doc/:id/audit). Pure: the page
 * renders what these return. `details` comes from the server's per-action
 * whitelist; anything missing (an old row, a newer server) still reads.
 */
import type { AuditAuthor, DocAuditEvent, DocAuditStats } from "../../lib/validators";

export type AuditEventCategory = "exports" | "access" | "changes" | "other";

/** The actions in each filter, as the server groups them. */
const ACTIONS_BY_CATEGORY: Record<Exclude<AuditEventCategory, "other">, readonly string[]> = {
  exports: ["doc.export"],
  access: ["doc.grant", "doc.grant.delete", "admin.grant", "doc.restrict", "doc.unrestrict"],
  changes: [
    "doc.create",
    "doc.rename",
    "doc.move",
    "doc.duplicate",
    "doc.archive",
    "doc.restore",
    "doc.purge",
    "doc.template.mark",
    "doc.template.unmark",
  ],
};

export function categoryOfAction(action: string): AuditEventCategory {
  for (const category of ["exports", "access", "changes"] as const) {
    if (ACTIONS_BY_CATEGORY[category].includes(action)) return category;
  }
  return "other";
}

/** A sentence about one event: `actor` (shown bold), then `text`, then an
 *  optional link to another page and the text after it. */
export type AuditSentence = {
  actor: string;
  text: string;
  link?: { to: string; title: string };
  textAfterLink?: string;
};

const FORMER_MEMBER = "Former member";

/** A person's name, "CI bot (bot)" for a bot, or "Former member" for a
 *  former, anonymized or missing one. `bot` is checked as `=== true`, since
 *  a grant row's person comes from the unvalidated `details`. */
export function personLabel(person: AuditAuthor | Omit<AuditAuthor, "bot"> | null): string {
  if (!person || person.former || !person.name) return FORMER_MEMBER;
  return isBot(person) ? `${person.name} (bot)` : person.name;
}

function isBot(person: AuditAuthor | Omit<AuditAuthor, "bot">): boolean {
  return "bot" in person && person.bot === true;
}

/** Who did it: a person, or the kind of process when no person did. */
export function actorLabel(actor: AuditAuthor | null, actorKind: string): string {
  if (actorKind === "user") return personLabel(actor);
  if (actorKind === "admin_cli") return "An administrator (command line)";
  if (actorKind === "retention") return "Retention job";
  return "System";
}

const FORMAT_SENTENCE_LABELS: Record<string, string> = {
  docx: "Word (.docx)",
  odt: "OpenDocument (.odt)",
  markdown_zip: "Markdown (zip)",
};

const FORMAT_SHORT_LABELS: Record<string, string> = {
  docx: "Word",
  odt: "OpenDocument",
  markdown_zip: "Markdown (zip)",
};

/** "Word", "OpenDocument", "Markdown (zip)"; an unknown format as it is. */
export function exportFormatShortLabel(format: string | null | undefined): string {
  if (!format) return "Unknown format";
  return FORMAT_SHORT_LABELS[format] ?? format;
}

function stringField(details: Record<string, unknown>, key: string): string | null {
  const value = details[key];
  return typeof value === "string" ? value : null;
}

function plural(count: number, singular: string, pluralForm = `${singular}s`): string {
  return `${count} ${count === 1 ? singular : pluralForm}`;
}

const VIA_API_KEY = ", with an API key";

/** The export sentence, ending in ", with an API key" when the server says one was used. */
function describeExport(details: Record<string, unknown>): Omit<AuditSentence, "actor"> {
  const sentence = describeExportedContent(details);
  if (details.via_api_key !== true) return sentence;
  if (sentence.textAfterLink !== undefined) {
    return { ...sentence, textAfterLink: `${sentence.textAfterLink}${VIA_API_KEY}` };
  }
  return { ...sentence, text: `${sentence.text}${VIA_API_KEY}` };
}

function describeExportedContent(details: Record<string, unknown>): Omit<AuditSentence, "actor"> {
  const format = stringField(details, "format");
  const scope = stringField(details, "scope");
  const formatLabel = format ? (FORMAT_SENTENCE_LABELS[format] ?? format) : null;
  const asFormat = formatLabel ? ` as ${formatLabel}` : "";

  if (scope === "workspace") return { text: "exported the whole workspace, including this page" };
  if (scope === "subtree") {
    // The root's own row has no `root` key; a subpage's row names its root
    // when the reader can open it.
    if (!("root" in details)) return { text: `exported this page and its subpages${asFormat}` };
    const root = details.root as { id?: unknown; title?: unknown } | null;
    if (details.root_hidden === true || !root || typeof root.id !== "string") {
      return { text: "exported this page as part of a page you can't open and its subpages" };
    }
    return {
      text: "exported this page as part of",
      link: { to: `/doc/${root.id}`, title: typeof root.title === "string" ? root.title : "a page" },
      textAfterLink: "and its subpages",
    };
  }
  const diagramCount = typeof details.diagram_pictures === "number" ? details.diagram_pictures : 0;
  const withDiagrams = diagramCount > 0 ? `, with ${plural(diagramCount, "diagram")}` : "";
  return { text: `exported this page${asFormat}${withDiagrams}` };
}

type Principal = { kind: "user"; label: string; bot: boolean } | { kind: "group"; label: string };

/** The person or group a grant row names; null for an old row without one. */
function principalOf(details: Record<string, unknown>): Principal | null {
  const principal = details.principal as { kind?: unknown; person?: unknown; name?: unknown } | undefined;
  if (!principal || typeof principal !== "object") return null;
  if (principal.kind === "user") {
    const person = principal.person as AuditAuthor | null | undefined;
    const label = personLabel(person ?? null);
    if (label === FORMER_MEMBER) return { kind: "user", label: "a former member", bot: false };
    return { kind: "user", label, bot: person ? isBot(person) : false };
  }
  if (principal.kind === "group" && typeof principal.name === "string") {
    return { kind: "group", label: `group ${principal.name}` };
  }
  return null;
}

/** "gave Cy editor access"; `markThisPageOnly` adds "(this page only)" for
 *  a grant that doesn't reach the subpages. */
function describeGrant(
  details: Record<string, unknown>,
  verb: "gave" | "granted",
  markThisPageOnly: boolean,
): string | null {
  const principal = principalOf(details);
  const role = stringField(details, "role");
  if (!principal || !role) return null;
  const thisPageOnly = markThisPageOnly && details.inherit === false ? " (this page only)" : "";
  return `${verb} ${principal.label} ${role} access${thisPageOnly}`;
}

const CHANGE_SENTENCES: Record<string, string> = {
  "doc.restrict": "restricted this page",
  "doc.unrestrict": "lifted the restriction",
  "doc.create": "created this page",
  "doc.rename": "renamed this page",
  "doc.move": "moved this page",
  "doc.duplicate": "created this page as a copy",
  "doc.archive": "moved this page to the archive",
  "doc.restore": "restored this page",
  "doc.purge": "deleted this page for good",
  "doc.template.mark": "saved this page as a template",
  "doc.template.unmark": "removed it from templates",
};

export function describeAuditEvent(event: DocAuditEvent): AuditSentence {
  const actor = actorLabel(event.actor, event.actor_kind);
  const { details } = event;
  switch (event.action) {
    case "doc.export":
      return { actor, ...describeExport(details) };
    case "doc.grant":
      return { actor, text: describeGrant(details, "gave", true) ?? "changed access to this page" };
    case "admin.grant": {
      // One parenthesis is enough: the break-glass mark says more than the reach.
      const granted = describeGrant(details, "granted", false) ?? "granted access to this page";
      return { actor, text: `${granted} (break-glass)` };
    }
    case "doc.grant.delete": {
      const principal = principalOf(details);
      if (!principal) return { actor, text: "removed access to this page" };
      // "CI bot (bot)'s access" doesn't read; a bot takes the long form.
      const possessive = principal.kind === "user" && !principal.bot;
      return {
        actor,
        text: possessive ? `removed ${principal.label}'s access` : `removed the access of ${principal.label}`,
      };
    }
    default:
      return { actor, text: CHANGE_SENTENCES[event.action] ?? event.action };
  }
}

const FORMAT_ORDER = ["docx", "odt", "markdown_zip"];

/** The export counts per format, known formats first, zero counts left out. */
export function formatBreakdown(
  byFormat: DocAuditStats["exports"]["by_format"],
): Array<{ format: string; label: string; count: number }> {
  const formats = [
    ...FORMAT_ORDER.filter((format) => format in byFormat),
    ...Object.keys(byFormat).filter((format) => !FORMAT_ORDER.includes(format)),
  ];
  return formats
    .map((format) => ({ format, label: exportFormatShortLabel(format), count: byFormat[format] ?? 0 }))
    .filter((entry) => entry.count > 0);
}

/** "1 as part of a subpage export · 1 in a workspace export", or null when
 *  every export was of this page alone. */
export function scopeSummary(byScope: DocAuditStats["exports"]["by_scope"]): string | null {
  const parts: string[] = [];
  const subtreeCount = byScope.subtree ?? 0;
  const workspaceCount = byScope.workspace ?? 0;
  if (subtreeCount > 0) parts.push(`${subtreeCount} as part of a subpage export`);
  if (workspaceCount > 0) parts.push(`${workspaceCount} in a workspace export`);
  return parts.length > 0 ? parts.join(" · ") : null;
}

const OTHER_ACTIVITY_LABELS: Record<Exclude<AuditEventCategory, "exports">, string> = {
  access: "Access changes",
  changes: "Changes",
  other: "Other",
};

/** Event counts that aren't exports, summed per category, empty ones left out. */
export function otherActivityBreakdown(
  byAction: DocAuditStats["by_action"],
): Array<{ category: Exclude<AuditEventCategory, "exports">; label: string; count: number }> {
  const countsByCategory = { access: 0, changes: 0, other: 0 };
  for (const entry of byAction) {
    const category = categoryOfAction(entry.action);
    if (category !== "exports") countsByCategory[category] += entry.count;
  }
  return (["access", "changes", "other"] as const)
    .map((category) => ({ category, label: OTHER_ACTIVITY_LABELS[category], count: countsByCategory[category] }))
    .filter((entry) => entry.count > 0);
}

export function exportCountText(count: number): string {
  return plural(count, "export");
}

export function eventCountText(count: number): string {
  return plural(count, "event");
}
