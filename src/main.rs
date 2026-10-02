//! Command line entry point.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use forge_bot::agent::AgentRegistry;
use forge_bot::config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "forge-bot",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FORGE_BOT_COMMIT"), ")"),
    about = "Route forge @agent mentions to coding agents"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "FORGE_BOT_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the webhook server (default).
    Serve,
    /// Run only the polling ingester.
    Poll,
    /// Validate the configuration and print a summary.
    Check,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    forge_bot::init_tracing();

    let config = Config::load(cli.config.as_deref())?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => forge_bot::serve(config).await?,
        Command::Poll => forge_bot::poll(config).await?,
        Command::Check => print_summary(&config)?,
    }

    Ok(())
}

fn print_summary(config: &Config) -> anyhow::Result<()> {
    println!("bind:          {}", config.bind);
    println!("mention:       {}", config.mention);
    let agents = AgentRegistry::from_config(config);
    println!("default agent: {}", agents.default_name());
    println!("agents:        {}", agents.names().join(", "));

    let forges = forge_bot::build_adapters(config);
    let mut names: Vec<_> = forges.keys().cloned().collect();
    names.sort();
    println!("forges:        {}", names.join(", "));

    if names.is_empty() {
        eprintln!("warning: no forges are configured; the server will reject every webhook");
    }
    if config.policy.allowed_users.is_empty()
        && config.policy.allowed_repos.is_empty()
        && !config.policy.allow_all
    {
        eprintln!(
            "warning: the policy allows nobody; set allowed_users/allowed_repos or allow_all"
        );
    }
    if let Some(forgejo) = &config.forges.forgejo
        && forgejo.webhook_secret.is_none()
    {
        eprintln!("warning: forgejo webhook secret is not set; signatures will not be verified");
    }

    // Resolve the explicit users and validate their Linux accounts, so a typo
    // is reported by `check` instead of at the first mention.
    let identities = forge_bot::identity::Identities::resolve(config, &agents.names())?;
    let executor = forge_bot::executor::Executor::from_config(&config.executor)?;
    executor.validate_users(&identities)?;
    // The cgroup executor forks natively and needs a writable cgroup v2
    // hierarchy; fail fast when it is missing instead of letting every
    // mention fail at spawn time.
    executor.ensure_cgroup_root()?;
    let global_token = config
        .forges
        .forgejo
        .as_ref()
        .and_then(|forgejo| forgejo.token.as_deref());
    for user in identities.users() {
        if user.role == forge_bot::config::UserRole::Reviewer
            && user.effective_token(global_token).is_none()
        {
            eprintln!(
                "warning: user `{}` has no token; it cannot reply or use the API",
                user.id
            );
        }
    }
    let users: Vec<String> = identities
        .users()
        .iter()
        .map(|user| format!("{}={}", user.id, user.host_user))
        .collect();
    println!("users:         {}", users.join(", "));
    Ok(())
}
