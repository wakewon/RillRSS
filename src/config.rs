//! Bootstrap configuration from environment variables / CLI flags.
//! Everything else is stored in the database and editable in the web admin.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;

#[derive(Debug, Clone, Args)]
pub struct Config {
    /// Directory holding the database, image cache and other state.
    #[arg(long, env = "RILL_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, env = "RILL_LISTEN", default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// Public base URL (e.g. https://rss.example.com). Used for cached-image URLs.
    /// When empty it is derived from each request (Host / X-Forwarded-* headers).
    #[arg(long, env = "RILL_BASE_URL", default_value = "")]
    pub base_url: String,

    /// Admin / API username.
    #[arg(long, env = "RILL_USERNAME", default_value = "admin")]
    pub username: String,

    /// Initial password. Only used when no password is stored yet (first start);
    /// afterwards change it in the web admin. If unset on first start, a random
    /// password is generated and printed to the log.
    #[arg(long, env = "RILL_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// Trust X-Forwarded-For / X-Forwarded-Proto / X-Forwarded-Host headers.
    #[arg(long, env = "RILL_TRUST_PROXY", default_value_t = true, action = clap::ArgAction::Set)]
    pub trust_proxy: bool,

    /// Allow article pages and images to be fetched from private/loopback networks.
    /// Feeds themselves (added by the admin) may always use private addresses.
    #[arg(long, env = "RILL_ALLOW_PRIVATE_NETWORKS", default_value_t = false, action = clap::ArgAction::Set)]
    pub allow_private_networks: bool,

    /// Default OpenAI-compatible API base URL (used when not set in the web admin).
    #[arg(long, env = "RILL_TRANSLATE_BASE_URL", default_value = "")]
    pub translate_base_url: String,

    /// Default API key for the translation service.
    #[arg(long, env = "RILL_TRANSLATE_API_KEY", default_value = "", hide_env_values = true)]
    pub translate_api_key: String,

    /// Default translation model.
    #[arg(long, env = "RILL_TRANSLATE_MODEL", default_value = "")]
    pub translate_model: String,

    /// Disable background workers (fetching/processing). Useful for maintenance.
    #[arg(long, env = "RILL_NO_WORKERS", default_value_t = false, action = clap::ArgAction::Set)]
    pub no_workers: bool,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("rillrss.db")
    }
    pub fn images_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    /// Minimal config for tests.
    pub fn for_tests(data_dir: PathBuf) -> Config {
        Config {
            data_dir,
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: String::new(),
            username: "admin".into(),
            password: Some("password123".into()),
            trust_proxy: true,
            allow_private_networks: true,
            translate_base_url: String::new(),
            translate_api_key: String::new(),
            translate_model: String::new(),
            no_workers: false,
        }
    }
}
