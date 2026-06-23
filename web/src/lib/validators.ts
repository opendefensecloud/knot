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

export function parse<T>(schema: v.GenericSchema<T>, data: unknown): T {
  return v.parse(schema, data);
}
