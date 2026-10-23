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

export function parse<T>(schema: v.GenericSchema<T>, data: unknown): T {
  return v.parse(schema, data);
}
