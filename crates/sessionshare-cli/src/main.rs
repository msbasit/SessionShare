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
    }

    Ok(())
}
