import * as v from "valibot";

const sessionSchema = v.object({
  user_id: v.string(),
  email: v.string(),
  display_name: v.string(),
  workspace_id: v.string(),
  role: v.picklist(["owner", "editor", "viewer"]),
});
export type Session = v.InferOutput<typeof sessionSchema>;
export const Session = sessionSchema;

const authConfigSchema = v.object({
  setup_available: v.boolean(),
  oidc_enabled: v.boolean(),
  password_login_enabled: v.boolean(),
});
export type AuthConfig = v.InferOutput<typeof authConfigSchema>;
export const AuthConfig = authConfigSchema;

const workspaceSchema = v.object({
  id: v.string(),
  slug: v.string(),
  name: v.string(),
  role: v.picklist(["owner", "editor", "viewer"]),
  /** Days an archived doc stays restorable before the server purges it. */
  archive_retention_days: v.fallback(v.number(), 30),
});
export type Workspace = v.InferOutput<typeof workspaceSchema>;
export const Workspace = workspaceSchema;

const memberSchema = v.object({
  user_id: v.string(),
  email: v.string(),
  display_name: v.string(),
  role: v.picklist(["owner", "editor", "viewer"]),
  // Older servers send no kind: everyone is human there.
  kind: v.optional(v.picklist(["human", "bot"]), "human"),
});
export type Member = v.InferOutput<typeof memberSchema>;
export const Member = memberSchema;

const docSchema = v.object({
  id: v.string(),
  workspace_id: v.string(),
  parent_id: v.nullable(v.string()),
  title: v.string(),
  sort_key: v.string(),
  icon: v.nullable(v.string()),
  created_by: v.string(),
  archived: v.boolean(),
  is_template: v.fallback(v.boolean(), false),
  archived_at: v.fallback(v.nullable(v.string()), null),
  /** The doc the user archived; set on it and on every subpage that went with it. */
  archive_root_id: v.fallback(v.nullable(v.string()), null),
  /** The doc is a restriction boundary: only people granted access see it. */
  restricted: v.fallback(v.boolean(), false),
  /** The doc or an ancestor is a restriction boundary. */
  within_restricted: v.fallback(v.boolean(), false),
  /** The parent is a page the user can't see; `parent_id` is then null and
   *  the sidebar lists the doc under "Shared with you". */
  parent_hidden: v.fallback(v.boolean(), false),
  /** The user's role on this doc (GET /api/docs only; null elsewhere). */
  effective_role: v.fallback(v.nullable(v.picklist(["owner", "editor", "viewer"])), null),
});
export type Doc = v.InferOutput<typeof docSchema>;
export const Doc = docSchema;

const personRefSchema = v.object({ id: v.string(), display_name: v.string() });

const docWithRoleSchema = v.object({
  id: v.string(),
  workspace_id: v.string(),
  parent_id: v.nullable(v.string()),
  title: v.string(),
  sort_key: v.string(),
  icon: v.nullable(v.string()),
  created_by: v.string(),
  archived: v.boolean(),
  is_template: v.fallback(v.boolean(), false),
  effective_role: v.picklist(["owner", "editor", "viewer"]),
  // Archive state for the banner; all null on a live doc.
  archived_at: v.fallback(v.nullable(v.string()), null),
  archived_by: v.fallback(v.nullable(personRefSchema), null),
  purge_at: v.fallback(v.nullable(v.string()), null),
  archive_root_id: v.fallback(v.nullable(v.string()), null),
  archive_root_title: v.fallback(v.nullable(v.string()), null),
  restricted: v.fallback(v.boolean(), false),
  within_restricted: v.fallback(v.boolean(), false),
  /** The nearest restriction boundary (this doc or an ancestor); null
   *  outside a restricted subtree and when the user can't see it. */
  restriction_root_id: v.fallback(v.nullable(v.string()), null),
});
export type DocWithRole = v.InferOutput<typeof docWithRoleSchema>;
export const DocWithRole = docWithRoleSchema;

/** One row of GET /api/workspace/archive: a doc someone moved to the archive. */
const archiveEntrySchema = v.object({
  id: v.string(),
  title: v.string(),
  parent_id: v.nullable(v.string()),
  parent_title: v.nullable(v.string()),
  subpage_count: v.number(),
  archived_at: v.string(),
  archived_by: v.nullable(personRefSchema),
  purge_at: v.string(),
  /** A permanent deletion has started and not finished; it can't be restored. */
  purging: v.fallback(v.boolean(), false),
  /** The parent is a page the user can't see; `parent_id` and
   *  `parent_title` are then null, as they are for a top-level page. */
  parent_hidden: v.fallback(v.boolean(), false),
  /** The user's role on this page, which a restricted page sets apart
   *  from the workspace role; null when the server doesn't say. */
  effective_role: v.fallback(v.nullable(v.picklist(["owner", "editor", "viewer"])), null),
  /** The subpages archived with it that the user can open. */
  subpage_ids: v.fallback(v.array(v.string()), []),
});
export type ArchiveEntry = v.InferOutput<typeof archiveEntrySchema>;
export const ArchiveEntry = archiveEntrySchema;

const grantSchema = v.object({
  principal: v.string(),
  role: v.picklist(["owner", "editor", "viewer"]),
  inherit: v.boolean(),
});
export type Grant = v.InferOutput<typeof grantSchema>;
export const Grant = grantSchema;

/**
 * GET /api/docs/{id}/access: who can see the doc, for the permissions
 * dialog. `people` are the grants that apply: the doc's own, then the
 * inheriting grants of its ancestors, nearest first, stopping at the
 * restriction boundary. Docs the user can't read are never named, so a
 * `source_*` or `restriction_root_*` pair is null for them.
 */
const docAccessPersonSchema = v.object({
  principal: v.string(),
  role: v.picklist(["owner", "editor", "viewer"]),
  inherit: v.boolean(),
  source_doc_id: v.nullable(v.string()),
  source_title: v.nullable(v.string()),
});
export type DocAccessPerson = v.InferOutput<typeof docAccessPersonSchema>;

const docAccessInfoSchema = v.object({
  restricted: v.boolean(),
  restriction_root_id: v.nullable(v.string()),
  restriction_root_title: v.nullable(v.string()),
  people: v.array(docAccessPersonSchema),
});
export type DocAccessInfo = v.InferOutput<typeof docAccessInfoSchema>;
export const DocAccessInfo = docAccessInfoSchema;

/**
 * GET /api/docs/{id}/contributors. Names only — the server joins `users` for
 * display names and never sends emails, so neither does this schema.
 * `contributors` arrives most-recent-first; the client keeps that order.
 * `contributors_since` is when tracking began for this doc: edits made before
 * it were never attributed, so the byline says so instead of implying a
 * complete list.
 */
const docContributorSchema = v.object({
  user_id: v.string(),
  display_name: v.string(),
  first_edited_at: v.string(),
  last_edited_at: v.string(),
});
export type DocContributor = v.InferOutput<typeof docContributorSchema>;

const docContributorsSchema = v.object({
  created_by: v.object({ id: v.string(), display_name: v.string() }),
  created_at: v.string(),
  contributors_since: v.string(),
  contributors: v.array(docContributorSchema),
});
export type DocContributors = v.InferOutput<typeof docContributorsSchema>;
export const DocContributors = docContributorsSchema;

/**
 * GET /api/whats-new: the running build, and the releases this user hasn't
 * seen yet. `dev` is a build without a release version; it never has
 * unseen releases. `unseen` is newest first.
 */
const whatsNewSchema = v.object({
  version: v.string(),
  commit: v.string(),
  released_on: v.nullable(v.string()),
  dev: v.boolean(),
  last_seen_version: v.nullable(v.string()),
  unseen: v.array(v.string()),
});
export type WhatsNew = v.InferOutput<typeof whatsNewSchema>;
export const WhatsNew = whatsNewSchema;

/**
 * GET /api/release-notes: CHANGELOG.md, newest release first. Each section
 * is one "###" heading of a release with its body as HTML; text straight
 * under the release heading comes as a section with an empty title.
 */
const releaseNotesSchema = v.object({
  releases: v.array(
    v.object({
      version: v.string(),
      date: v.nullable(v.string()),
      sections: v.array(v.object({ title: v.string(), html: v.string() })),
    }),
  ),
});
export type ReleaseNotes = v.InferOutput<typeof releaseNotesSchema>;
export const ReleaseNotes = releaseNotesSchema;

const diagramListSchema = v.object({
  limits: v.object({
    max_body_bytes: v.number(),
    max_part_bytes: v.number(),
    max_parts: v.number(),
    scale: v.number(),
  }),
  diagrams: v.array(
    v.variant("kind", [
      v.object({ kind: v.literal("mermaid"), key: v.string(), source: v.string() }),
      v.object({
        kind: v.literal("board"),
        key: v.string(),
        board_id: v.string(),
        label: v.string(),
      }),
    ]),
  ),
});
export type DiagramList = v.InferOutput<typeof diagramListSchema>;
export const DiagramList = diagramListSchema;

/**
 * A person on the page audit log (GET /api/docs/{id}/audit), as the page
 * history shows them: `key` and `name` are null for a former or anonymized
 * member, who reads "Former member". Never an email. `bot` is true for a
 * bot account; the server sends it only then, so it defaults to false.
 */
const auditAuthorSchema = v.object({
  key: v.nullable(v.string()),
  name: v.nullable(v.string()),
  former: v.boolean(),
  bot: v.fallback(v.boolean(), false),
});
export type AuditAuthor = v.InferOutput<typeof auditAuthorSchema>;
export const AuditAuthor = auditAuthorSchema;

/**
 * One audited event on a page, newest first. `category` is `exports`,
 * `access`, `changes` or `other`; `actor_kind` is `user`, `admin_cli`,
 * `retention` or `system` (kept as plain strings so a newer server's value
 * still renders). `details` is the server's per-action whitelist; the raw
 * audit data never reaches the client (a `doc.export` made with an API key
 * has `via_api_key: true`, never the key). `id` is an opaque token, never
 * the stored event id, and is what `cursor` takes back.
 */
const docAuditEventSchema = v.object({
  id: v.string(),
  at: v.string(),
  action: v.string(),
  category: v.string(),
  actor_kind: v.string(),
  actor: v.nullable(auditAuthorSchema),
  details: v.fallback(v.record(v.string(), v.unknown()), {}),
});
export type DocAuditEvent = v.InferOutput<typeof docAuditEventSchema>;
export const DocAuditEvent = docAuditEventSchema;

const docAuditStatsSchema = v.object({
  total: v.number(),
  by_action: v.array(v.object({ action: v.string(), count: v.number(), last_at: v.string() })),
  exports: v.object({
    total: v.number(),
    /** Distinct exporters; every former member counts as one. */
    people: v.number(),
    last_30_days: v.number(),
    last: v.nullable(
      v.object({
        at: v.string(),
        actor_kind: v.string(),
        actor: v.nullable(auditAuthorSchema),
        format: v.fallback(v.nullable(v.string()), null),
        scope: v.fallback(v.nullable(v.string()), null),
      }),
    ),
    /** `docx`, `odt`, `markdown_zip`; zero counts are left out. */
    by_format: v.fallback(v.record(v.string(), v.number()), {}),
    /** `page`, `subtree`, `workspace`; zero counts are left out. */
    by_scope: v.fallback(v.record(v.string(), v.number()), {}),
    /** Most recent exporter first, at most 50. */
    by_person: v.array(v.object({ actor: auditAuthorSchema, count: v.number(), last_at: v.string() })),
    by_person_truncated: v.fallback(v.boolean(), false),
  }),
});
export type DocAuditStats = v.InferOutput<typeof docAuditStatsSchema>;
export const DocAuditStats = docAuditStatsSchema;

/**
 * One page of a page's audit log. `stats` is only on the first page;
 * `next_cursor` is the opaque id of its last event while older ones remain.
 */
const docAuditPageSchema = v.object({
  events: v.array(docAuditEventSchema),
  next_cursor: v.nullable(v.string()),
  stats: v.nullable(docAuditStatsSchema),
});
export type DocAuditPage = v.InferOutput<typeof docAuditPageSchema>;
export const DocAuditPage = docAuditPageSchema;

/**
 * One of the user's favorite pages (GET /api/me/favorites), oldest star
 * first. Only pages the user can read that are neither archived nor
 * templates are listed.
 */
const favoriteSchema = v.object({
  doc_id: v.string(),
  title: v.string(),
  icon: v.nullable(v.string()),
  favorited_at: v.string(),
});
export type Favorite = v.InferOutput<typeof favoriteSchema>;
export const Favorite = favoriteSchema;

export function parse<T>(schema: v.GenericSchema<unknown, T>, data: unknown): T {
  return v.parse(schema, data);
}
