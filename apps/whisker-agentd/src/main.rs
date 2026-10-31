//! whisker-agentd: manage Whisker accounts and run AI agents headless,
//! e.g. on a server next to your homeserver.
//!
//! Don't run it at the same time as the GUI on the same data directory.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::path::PathBuf;
use whisker_agent::{AgentConfig, ApiConfig, Llm, Permission, Trigger, agents_in, detect, save_agents, start_all};
use whisker_core::{AccountId, AccountKind, AccountManager, LoginRequest};

#[derive(Parser)]
#[command(name = "whisker-agentd", version, about = "Headless Whisker: accounts and AI agents")]
struct Cli {
    /// Data directory (defaults to the same one the GUI uses).
    #[arg(long, env = "WHISKER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a vault. Run again with a different passphrase for a hidden vault.
    Init,
    /// Log an account in and store it in the vault the passphrase opens.
    Login {
        #[arg(long)]
        homeserver: String,
        #[arg(long)]
        user: String,
        /// Mark it as an automated (AI) account.
        #[arg(long)]
        agent: bool,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// e.g. socks5h://127.0.0.1:9050
        #[arg(long)]
        proxy: Option<String>,
    },
    /// List unlocked accounts.
    Accounts,
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Unlock and run every enabled agent until stopped.
    Run,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Attach an AI agent to an account.
    Add {
        /// Account id (from `accounts`).
        #[arg(long)]
        account: AccountId,
        /// The AI API's address: any OpenAI- or Anthropic-style server,
        /// e.g. openrouter.ai/api, localhost:11434 (Ollama), 127.0.0.1:8080 (llama.cpp).
        #[arg(long)]
        api_url: String,
        /// Read the API key from this environment variable (if it needs one).
        #[arg(long)]
        api_key_env: Option<String>,
        /// Model name; omit to list the models the API offers.
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated: reply,redact,kick,ban,unban,invite,set_power_level
        #[arg(long, default_value = "reply", value_delimiter = ',')]
        grant: Vec<String>,
        /// Users allowed to order moderation, regardless of power level.
        #[arg(long = "commander")]
        commanders: Vec<String>,
        /// mention | every | direct-only
        #[arg(long, default_value = "mention")]
        trigger: String,
        #[arg(long)]
        audit_room: Option<String>,
        #[arg(long)]
        system_prompt: Option<String>,
    },
    List,
    Remove {
        #[arg(long)]
        account: AccountId,
    },
}

fn passphrase(prompt: &str) -> Result<String> {
    if let Ok(p) = std::env::var("WHISKER_PASSPHRASE") {
        return Ok(p);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

fn default_data_dir() -> Result<PathBuf> {
    directories::ProjectDirs::from("io.github", "anti-ghostsec", "Whisker")
        .map(|d| d.data_dir().to_owned())
        .context("no home directory; pass --data-dir")
}

/// Unlocks and returns the single vault the passphrase opened.
async fn unlock(mgr: &AccountManager) -> Result<String> {
    let opened = mgr.unlock(&passphrase("Vault passphrase: ")?).await?;
    match opened.as_slice() {
        [] => bail!("no vault opens with that passphrase"),
        [one] => Ok(one.clone()),
        _ => bail!("that passphrase opens more than one vault; use distinct passphrases"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,matrix_sdk=warn".into()))
        .init();
    let cli = Cli::parse();
    let mgr = AccountManager::new(match cli.data_dir {
        Some(d) => d,
        None => default_data_dir()?,
    });

    let result: Result<()> = async {
    match cli.cmd {
        Cmd::Init => {
            let a = passphrase("New vault passphrase: ")?;
            if std::env::var("WHISKER_PASSPHRASE").is_err() && a != rpassword::prompt_password("Repeat: ")? {
                bail!("passphrases differ");
            }
            if !mgr.unlock(&a).await?.is_empty() {
                bail!("a vault already opens with that passphrase");
            }
            mgr.create_vault(&a).await?;
            println!("vault created in {}", mgr.data_dir().display());
        }
        Cmd::Login { homeserver, user, agent, tags, proxy } => {
            let vault = unlock(&mgr).await?;
            let password = rpassword::prompt_password(format!("Password for {user}: "))?;
            let id = mgr
                .login(LoginRequest {
                    vault: &vault,
                    register: false,
                    token: None,
                    homeserver: &homeserver,
                    username: &user,
                    password: &password,
                    kind: if agent { AccountKind::Agent } else { AccountKind::Human },
                    tags: tags.into_iter().collect(),
                    proxy,
                })
                .await?;
            println!("logged in: {id}");
        }
        Cmd::Accounts => {
            unlock(&mgr).await?;
            for a in mgr.accounts().await {
                let tags: Vec<_> = a.tags.iter().cloned().collect();
                println!("{}  {:<6}  {}  [{}]", a.id, format!("{:?}", a.kind).to_lowercase(), a.user_id, tags.join(", "));
            }
        }
        Cmd::Agent(AgentCmd::Add {
            account,
            api_url,
            api_key_env,
            model,
            grant,
            commanders,
            trigger,
            audit_room,
            system_prompt,
        }) => {
            let vault = unlock(&mgr).await?;
            let api_key = match api_key_env {
                Some(var) => Some(std::env::var(&var).with_context(|| format!("${var} is not set"))?),
                None => None,
            };
            let found = detect(&api_url, api_key.as_deref()).await?;
            println!("found {} ({:?}), {} models", found.service, found.format, found.models.len());
            let Some(model) = model else {
                for m in &found.models {
                    println!("  {m}");
                }
                bail!("pick one with --model");
            };
            let api = ApiConfig {
                url: api_url,
                api_key,
                model,
                format: found.format,
                endpoint: found.endpoint,
                service: found.service,
            };
            println!("testing… {}", Llm::new(api.clone(), 200, Default::default())?.test().await?);
            let mut cfg = AgentConfig::new(account, api);
            cfg.permissions.global = grant
                .iter()
                .map(|g| Permission::parse(g.trim()).with_context(|| format!("unknown permission {g}")))
                .collect::<Result<BTreeSet<_>>>()?;
            cfg.permissions.commanders.users = commanders
                .iter()
                .map(|c| c.as_str().try_into().with_context(|| format!("bad user id {c}")))
                .collect::<Result<_>>()?;
            cfg.trigger = match trigger.as_str() {
                "mention" => Trigger::Mention,
                "every" => Trigger::Every,
                "direct-only" => Trigger::DirectOnly,
                t => bail!("unknown trigger {t}"),
            };
            cfg.audit_room = audit_room.map(|r| r.as_str().try_into()).transpose().context("bad audit room id")?;
            if let Some(p) = system_prompt {
                cfg.system_prompt = p;
            }
            let mut agents = agents_in(&mgr, &vault).await;
            agents.retain(|a| a.account != account);
            agents.push(cfg);
            save_agents(&mgr, &vault, &agents).await?;
            println!("agent saved");
        }
        Cmd::Agent(AgentCmd::List) => {
            let vault = unlock(&mgr).await?;
            for a in agents_in(&mgr, &vault).await {
                let grants: Vec<_> = a.permissions.global.iter().map(|p| p.name()).collect();
                println!("{}  {:?}  enabled={}  grants={}", a.account, a.api, a.enabled, grants.join(","));
            }
        }
        Cmd::Agent(AgentCmd::Remove { account }) => {
            let vault = unlock(&mgr).await?;
            let mut agents = agents_in(&mgr, &vault).await;
            agents.retain(|a| a.account != account);
            save_agents(&mgr, &vault, &agents).await?;
        }
        Cmd::Run => {
            unlock(&mgr).await?;
            let handles = start_all(&mgr).await;
            if handles.is_empty() {
                bail!("no enabled agents in this vault");
            }
            println!("{} agent(s) running; Ctrl-C to stop", handles.len());
            tokio::signal::ctrl_c().await?;
        }
    }
    Ok(())
    }
    .await;
    // Locking packs each account's store into its vault and wipes the running copy.
    mgr.lock_all().await;
    result
}
