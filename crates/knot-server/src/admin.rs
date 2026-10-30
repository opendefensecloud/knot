//! `knot-server admin` — operator commands with direct database access.
//!
//! - `create` makes a local-password owner. It bootstraps the singleton
//!   workspace on first run, and doubles as a break-glass tool afterwards
//!   (mint a local owner when SSO is unavailable).
//! - `restricted-list` and `grant` recover restricted documents whose owners
//!   are gone: workspace owners have no implicit access inside a restricted
//!   subtree, so there is no in-app override.

use std::io::{Read, Write};

use clap::{Args, Subcommand};
use knot_auth::Hasher;
use knot_config::Config;
use knot_storage::{
    DocStore, GrantStore, GrantStoreError, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore,
    Pool, User, UserStore, WorkspaceRole, WorkspaceStore,
};
use uuid::Uuid;

#[derive(Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    pub cmd: AdminCmd,
}

#[derive(Subcommand)]
pub enum AdminCmd {
    /// Create a local-password owner, creating the singleton workspace if it
    /// doesn't exist yet. Usable for first-run bootstrap AND as a break-glass
    /// admin afterwards. Reads the password from stdin so it stays out of shell
    /// history. Refuses a duplicate email.
    Create {
        #[arg(long)]
        email: String,
        #[arg(long)]
        display_name: String,
        /// Workspace name to use when bootstrapping. Ignored if a workspace
        /// already exists.
        #[arg(long, default_value = "Workspace")]
        workspace_name: String,
        /// Workspace slug used at creation.
        #[arg(long, default_value = "default")]
        workspace_slug: String,
    },
    /// List the restriction boundaries of a workspace, one per line:
    /// `<doc_id>\t<title>\towners=<comma-separated emails>`.
    RestrictedList {
        /// Workspace slug.
        #[arg(long)]
        workspace: String,
    },
    /// Break-glass: grant a workspace member a role on a doc, e.g. owner on
    /// a restricted doc whose owners were all removed. Audited as
    /// `admin.grant` with `break_glass: true`.
    Grant {
        /// Id of the doc to grant on.
        #[arg(long)]
        doc: Uuid,
        /// Email of the user to grant; must be a member of the doc's
        /// workspace.
        #[arg(long)]
        email: String,
        /// owner, editor or viewer.
        #[arg(long)]
        role: String,
        /// Grant on this doc only, not its subpages.
        #[arg(long)]
        no_inherit: bool,
    },
}

pub async fn run(cfg: Config, args: AdminArgs) -> anyhow::Result<()> {
    match args.cmd {
        AdminCmd::Create {
            email,
            display_name,
            workspace_name,
            workspace_slug,
        } => create(cfg, &email, &display_name, &workspace_name, &workspace_slug).await,
        AdminCmd::RestrictedList { workspace } => {
            let pool = connect(&cfg).await?;
            for line in restricted_list(&pool, &workspace).await? {
                println!("{line}");
            }
            Ok(())
        }
        AdminCmd::Grant {
            doc,
            email,
            role,
            no_inherit,
        } => {
            let pool = connect(&cfg).await?;
            grant(&pool, doc, &email, &role, no_inherit).await?;
            println!("granted {role} on doc {doc} to {email}");
            Ok(())
        }
    }
}

async fn connect(cfg: &Config) -> anyhow::Result<Pool> {
    if cfg.database_url.is_empty() {
        anyhow::bail!("KNOT_DATABASE_URL must be set");
    }
    Ok(knot_storage::connect(&cfg.database_url, 4).await?)
}

async fn create(
    cfg: Config,
    email: &str,
    display_name: &str,
    workspace_name: &str,
    workspace_slug: &str,
) -> anyhow::Result<()> {
    let pool = connect(&cfg).await?;
    let users = PgUserStore::new(pool.clone());
    let ws = PgWorkspaceStore::new(pool);
    let hasher = Hasher::new();

    let mut buf = String::new();
    write!(std::io::stderr(), "password (read from stdin): ").ok();
    std::io::stderr().flush().ok();
    std::io::stdin().read_to_string(&mut buf)?;
    let password = buf.trim_end_matches(['\n', '\r']);

    let user = create_owner(
        &users,
        &ws,
        &hasher,
        email,
        display_name,
        password,
        workspace_slug,
        workspace_name,
    )
    .await?;

    let workspace = ws
        .get_singleton()
        .await?
        .expect("workspace exists after create_owner");
    println!(
        "created user {} ({}) as owner of workspace {} ({})",
        user.id, email, workspace.id, workspace.slug,
    );
    Ok(())
}

/// Create a local-password user and make them an owner of the singleton
/// workspace, creating that workspace if it doesn't exist yet.
///
/// This backs first-run bootstrap, but is also a **break-glass** tool: it works
/// even after the system already has users, so an operator can always mint a
/// local owner when SSO is down. It refuses only a duplicate email (which would
/// otherwise violate the `users.email` unique constraint).
#[allow(clippy::too_many_arguments)]
pub async fn create_owner(
    users: &dyn UserStore,
    ws: &dyn WorkspaceStore,
    hasher: &Hasher,
    email: &str,
    display_name: &str,
    password: &str,
    workspace_slug: &str,
    workspace_name: &str,
) -> anyhow::Result<User> {
    if password.len() < 8 {
        anyhow::bail!("password must be at least 8 characters");
    }
    if users.find_by_email(email).await?.is_some() {
        anyhow::bail!("a user with email {email} already exists");
    }

    let hash = hasher
        .hash(password)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let workspace = match ws.get_singleton().await? {
        Some(w) => w,
        None => ws.create(workspace_slug, workspace_name).await?,
    };
    let user = users.create_local(email, display_name, &hash).await?;
    ws.add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await?;
    Ok(user)
}

/// The restriction boundaries (docs with `restricted = true`, archived and
/// templates included) of the workspace `workspace_slug`, one line each:
/// `<doc_id>\t<title>\towners=<comma-separated emails>`, ordered by title.
/// Owners are the `user:` owner grants on the boundary doc itself; a
/// principal whose user is gone is printed as-is. An empty `owners=` marks
/// an orphaned subtree that needs `grant`.
pub async fn restricted_list(pool: &Pool, workspace_slug: &str) -> anyhow::Result<Vec<String>> {
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let docs = PgDocStore::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());

    let workspace = workspaces
        .get_singleton()
        .await?
        .filter(|workspace| workspace.slug == workspace_slug)
        .ok_or_else(|| anyhow::anyhow!("no workspace with slug {workspace_slug}"))?;

    let mut boundaries = Vec::new();
    for node in docs.acl_nodes(workspace.id).await? {
        if !node.restricted {
            continue;
        }
        let Some(doc) = docs.get(node.id).await? else {
            continue; // purged since acl_nodes ran
        };
        let mut owner_emails = Vec::new();
        for doc_grant in grants.list(doc.id).await? {
            if doc_grant.role != WorkspaceRole::Owner {
                continue;
            }
            let Some(user_id) = doc_grant.principal.strip_prefix("user:") else {
                continue;
            };
            let owner_user = match user_id.parse::<Uuid>() {
                Ok(user_id) => users.find_by_id(user_id).await?,
                Err(_) => None,
            };
            owner_emails.push(match owner_user {
                Some(user) => user.email,
                None => doc_grant.principal.clone(),
            });
        }
        owner_emails.sort();
        boundaries.push((doc.title, doc.id, owner_emails));
    }
    boundaries.sort();
    Ok(boundaries
        .into_iter()
        .map(|(title, doc_id, owner_emails)| {
            format!("{doc_id}\t{title}\towners={}", owner_emails.join(","))
        })
        .collect())
}

/// Break-glass grant: give the user `email` the `role` (owner, editor or
/// viewer) on `doc_id`, inheriting to subpages unless `no_inherit`.
///
/// The user must be a member of the doc's workspace. The grant goes through
/// `GrantStore::put_break_glass`, so the ACL invalidation fires and running
/// servers drop their cached access, the last-owner guard still applies,
/// and the membership check can't race a removal. There is no acting user:
/// the grant row's `granted_by` is NULL, and the only audit event is
/// `admin.grant` (actor `None`, `data.break_glass = true`), written in the
/// grant's transaction.
pub async fn grant(
    pool: &Pool,
    doc_id: Uuid,
    email: &str,
    role: &str,
    no_inherit: bool,
) -> anyhow::Result<()> {
    let docs = PgDocStore::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());

    let workspace_role = WorkspaceRole::parse(role)
        .ok_or_else(|| anyhow::anyhow!("role must be owner, editor or viewer, not {role}"))?;
    let doc = docs
        .get(doc_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no doc {doc_id}"))?;
    let user = users
        .find_by_email(email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user with email {email}"))?;
    let principal = format!("user:{}", user.id);
    match grants
        .put_break_glass(
            doc.workspace_id,
            doc_id,
            &principal,
            workspace_role,
            !no_inherit,
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(GrantStoreError::NotMember) => {
            anyhow::bail!("{email} is not a member of the doc's workspace")
        }
        Err(e) => Err(e.into()),
    }
}
