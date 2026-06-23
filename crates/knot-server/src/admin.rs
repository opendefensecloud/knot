//! `knot-server admin create` — create a local-password owner.
//!
//! Bootstraps the singleton workspace on first run, and doubles as a
//! break-glass tool afterwards (mint a local owner when SSO is unavailable).

use std::io::{Read, Write};

use clap::{Args, Subcommand};
use knot_auth::Hasher;
use knot_config::Config;
use knot_storage::{PgUserStore, PgWorkspaceStore, User, UserStore, WorkspaceRole, WorkspaceStore};

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
}

pub async fn run(cfg: Config, args: AdminArgs) -> anyhow::Result<()> {
    match args.cmd {
        AdminCmd::Create {
            email,
            display_name,
            workspace_name,
            workspace_slug,
        } => create(cfg, &email, &display_name, &workspace_name, &workspace_slug).await,
    }
}

async fn create(
    cfg: Config,
    email: &str,
    display_name: &str,
    workspace_name: &str,
    workspace_slug: &str,
) -> anyhow::Result<()> {
    if cfg.database_url.is_empty() {
        anyhow::bail!("KNOT_DATABASE_URL must be set");
    }
    let pool = knot_storage::connect(&cfg.database_url, 4).await?;
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
