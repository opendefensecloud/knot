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
});
export type Doc = v.InferOutput<typeof docSchema>;
export const Doc = docSchema;

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
});
export type DocWithRole = v.InferOutput<typeof docWithRoleSchema>;
export const DocWithRole = docWithRoleSchema;

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

export function parse<T>(schema: v.GenericSchema<T>, data: unknown): T {
  return v.parse(schema, data);
}
