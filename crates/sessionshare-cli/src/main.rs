use clap::{Parser, Subcommand};
use sessionshare_core::claude_fs::default_claude_dir;
use sessionshare_core::state_db::{default_app_data_dir, load_or_create_config};
use sessionshare_core::sync_engine::SyncEngine;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sessionshare", about = "Sync Claude Code sessions across Windows PCs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Configure the synced drive folder and machine name
    Init {
        #[arg(long)]
        sync_dir: PathBuf,
        #[arg(long)]
        machine_name: Option<String>,
        #[arg(long)]
        claude_dir: Option<PathBuf>,
    },
    /// List discovered local and remote projects
    Projects,
    /// Map a canonical remote project ID to a local folder path
    Map {
        canonical_id: String,
        local_path: String,
    },
    /// Run a bidirectional sync now
    Sync,
    /// List incoming hook / permission changes waiting for approval
    Pending,
    /// Approve the pending incoming change for a synced file (e.g. global/settings.json)
    Approve { remote_key: String },
    /// Reject the pending incoming change for a synced file
    Reject { remote_key: String },
    /// List files changed differently on both PCs (held until merged)
    Conflicts,
    /// Resolve a held conflict by keeping one PC's version on both PCs
    Keep {
        remote_key: String,
        /// `local` (this PC) or `remote` (other PC)
        #[arg(long, value_parser = ["local", "remote"])]
        side: String,
    },
}

fn resolve(engine: &SyncEngine, remote_key: &str, approve: bool) -> Result<(), Box<dyn std::error::Error>> {
    let pending = engine.pending_approvals()?;
    let item = pending
        .iter()
        .find(|p| p.remote_key == remote_key)
        .ok_or_else(|| format!("No pending change for {}", remote_key))?;
    engine.resolve_approval(&item.remote_key, &item.remote_hash, approve)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let app_data = default_app_data_dir();
    let mut cfg = load_or_create_config(&app_data, default_claude_dir())?;

    match cli.command {
        Commands::Init {
            sync_dir,
            machine_name,
            claude_dir,
        } => {
            cfg.sync_dir = Some(sync_dir);
            if let Some(name) = machine_name {
                cfg.machine_name = name;
            }
            if let Some(cdir) = claude_dir {
                cfg.claude_dir = cdir;
            }
            let mut engine = SyncEngine::new(app_data, cfg.clone())?;
            engine.update_config(cfg)?;
            println!("Initialized SessionShare.");
        }
        Commands::Projects => {
            let mut engine = SyncEngine::new(app_data, cfg)?;
            let projects = engine.discover_projects()?;
            println!("{}", serde_json::to_string_pretty(&projects)?);
        }
        Commands::Map {
            canonical_id,
            local_path,
        } => {
            let mut engine = SyncEngine::new(app_data, cfg)?;
            engine.map_project_local_path(&canonical_id, &local_path)?;
            println!("Mapped {} -> {}", canonical_id, local_path);
        }
        Commands::Sync => {
            let mut engine = SyncEngine::new(app_data, cfg)?;
            let report = engine.sync_all()?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Commands::Pending => {
            let engine = SyncEngine::new(app_data, cfg)?;
            println!("{}", serde_json::to_string_pretty(&engine.pending_approvals()?)?);
        }
        Commands::Approve { remote_key } => {
            let engine = SyncEngine::new(app_data, cfg)?;
            resolve(&engine, &remote_key, true)?;
            println!("Approved {}; it will be applied on the next sync.", remote_key);
        }
        Commands::Reject { remote_key } => {
            let engine = SyncEngine::new(app_data, cfg)?;
            resolve(&engine, &remote_key, false)?;
            println!("Rejected {}.", remote_key);
        }
        Commands::Conflicts => {
            let engine = SyncEngine::new(app_data, cfg)?;
            println!("{}", serde_json::to_string_pretty(&engine.conflicts()?)?);
        }
        Commands::Keep { remote_key, side } => {
            let engine = SyncEngine::new(app_data, cfg)?;
            let view = engine.conflict_view(&remote_key)?;
            let content = if side == "local" { &view.local_text } else { &view.remote_text };
            engine.resolve_conflict(&remote_key, &view.local_hash, &view.remote_hash, content)?;
            println!("Kept the {} version of {} on both PCs.", side, remote_key);
        }
    }

    Ok(())
}
