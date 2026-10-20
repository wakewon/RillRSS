use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use rillrss::config::Config;

#[derive(Parser)]
#[command(name = "rillrss", version, about = "Lightweight self-hosted RSS aggregator with Google Reader / TT-RSS APIs")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    cfg: Config,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default).
    Serve,
    /// Probe the local /healthz endpoint (for container health checks).
    Healthcheck {
        /// URL to probe; defaults to http://127.0.0.1:<port>/healthz
        #[arg(long)]
        url: Option<String>,
    },
    /// Set a new admin password (also invalidates sessions).
    ResetPassword {
        /// The new password.
        password: String,
    },
    /// Write a consistent copy of the database (safe while the server is running).
    Backup {
        /// Destination file, e.g. /data/backup-2026-10-20.db
        dest: std::path::PathBuf,
    },
    /// Run SQLite's integrity check on the database.
    CheckDb,
}

fn init_tracing() {
    let filter = std::env::var("RILL_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info,tower_http=warn,html5ever=error,selectors=error".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .init();
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Healthcheck { url }) => {
            let url = url.unwrap_or_else(|| format!("http://127.0.0.1:{}/healthz", cli.cfg.listen.port()));
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            let ok = rt.block_on(async {
                let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().ok()?;
                let r = client.get(&url).send().await.ok()?;
                Some(r.status().is_success())
            });
            if ok == Some(true) { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Some(Cmd::ResetPassword { password }) => {
            init_tracing();
            let rt = tokio::runtime::Runtime::new().expect("runtime");
            let res = rt.block_on(async {
                let mut cfg = cli.cfg.clone();
                cfg.no_workers = true;
                let app = rillrss::app::App::new(cfg).await?;
                rillrss::auth::set_password(&app, &password).await
            });
            match res {
                Ok(()) => {
                    println!("password updated");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Cmd::Backup { dest }) => {
            if dest.exists() {
                eprintln!("error: {} already exists", dest.display());
                return ExitCode::FAILURE;
            }
            match rillrss::db::Db::open(&cli.cfg.db_path()).and_then(|db| db.backup_to(&dest)) {
                Ok(()) => {
                    println!("backup written to {}", dest.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Cmd::CheckDb) => match rillrss::db::Db::open(&cli.cfg.db_path()).and_then(|db| db.integrity_check()) {
            Ok(r) => {
                println!("{r}");
                if r == "ok" { ExitCode::SUCCESS } else { ExitCode::FAILURE }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        None | Some(Cmd::Serve) => {
            init_tracing();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
            let res = rt.block_on(async {
                let app = rillrss::app::App::new(cli.cfg.clone()).await?;
                rillrss::server::run(app).await
            });
            match res {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::error!("fatal: {e:#}");
                    eprintln!("fatal: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
