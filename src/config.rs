use anyhow::{Context as _, Result, ensure};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about = "Evidence-backed property tax ledger")]
pub struct Config {
    #[arg(long, env = "PROPERTY_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    #[arg(long, env = "PROPERTY_DATABASE", default_value = "property.sqlite3")]
    pub database: PathBuf,
    #[arg(long, env = "PROPERTY_API_TOKEN", hide_env_values = true)]
    pub api_token: Option<String>,
    #[arg(long, env = "PROPERTY_INTEGRATION_TOKEN", hide_env_values = true)]
    pub integration_token: Option<String>,
    #[arg(long, env = "PROPERTY_INGRESS_BIND")]
    pub ingress_bind: Option<SocketAddr>,
    #[arg(long, env = "PROPERTY_INGRESS_PEER", default_value = "172.30.32.2")]
    pub ingress_peer: IpAddr,
    /// Installed app options file. App credentials live beside the database.
    #[arg(long)]
    pub options: Option<PathBuf>,
    /// Generate the schema from registered endpoints without starting a server.
    #[arg(long)]
    pub openapi: Option<PathBuf>,
}
impl Config {
    pub fn load() -> Result<Self> {
        let mut config = match Self::try_parse() {
            Ok(config) => config,
            Err(error)
                if matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                error.print()?;
                std::process::exit(0);
            }
            Err(_) => anyhow::bail!("invalid server arguments; use property --help"),
        };
        if config.openapi.is_some() {
            return Ok(config);
        }
        if let Some(path) = &config.options {
            // Options currently contain no ledger data or provider credentials.
            // Read and validate the object, never log its contents.
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .context("open app options")?
                .take(65_537)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65_536, "app options are too large");
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Options {}
            let _: Options = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid app options"))?;
            let dir = config
                .database
                .parent()
                .filter(|v| !v.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            let credentials = Credentials::load_or_create(&dir.join("credentials.json"))?;
            config.api_token = Some(credentials.owner);
            config.integration_token = Some(credentials.integration);
        }
        ensure!(
            config.api_token.as_deref().is_some_and(valid_token),
            "API credential must be 32..256 printable ASCII characters without whitespace"
        );
        ensure!(
            config.integration_token.as_deref().is_none_or(valid_token),
            "integration credential is invalid"
        );
        ensure!(
            config.integration_token.as_ref() != config.api_token.as_ref(),
            "owner and integration credentials must be distinct"
        );
        Ok(config)
    }
    pub fn sync_database(&self) -> PathBuf {
        self.database.with_extension("sync.sqlite3")
    }
}
#[derive(Serialize, Deserialize)]
struct Credentials {
    owner: String,
    integration: String,
}
impl Credentials {
    fn load_or_create(path: &std::path::Path) -> Result<Self> {
        use std::{
            fs::OpenOptions,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };
        if !path.try_exists().context("check private app credentials")? {
            let credentials = Self {
                owner: random_token()?,
                integration: random_token()?,
            };
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .context("create private app credentials")?;
            let result = (|| -> Result<()> {
                file.write_all(&serde_json::to_vec(&credentials)?)?;
                file.sync_all()?;
                match std::fs::hard_link(&temporary, path) {
                    Ok(()) => std::fs::File::open(path.parent().context("credential directory")?)?
                        .sync_all()?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => anyhow::bail!("publish private app credentials"),
                }
                Ok(())
            })();
            std::fs::remove_file(&temporary).context("remove temporary credentials")?;
            result?;
        }
        let metadata = std::fs::symlink_metadata(path).context("read credential permissions")?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "app credentials must be an owner-only regular file"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .context("open private credentials")?
            .take(4097)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "invalid private app credentials");
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid private app credentials"))?;
        ensure!(
            valid_token(&value.owner)
                && valid_token(&value.integration)
                && value.owner != value.integration,
            "invalid private app credentials"
        );
        Ok(value)
    }
}
fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len()) && token.bytes().all(|v| v.is_ascii_graphic())
}
fn random_token() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate private credential"))?;
    Ok(hex::encode(bytes))
}
#[cfg(test)]
mod tests {
    use crate::config::Credentials;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn app_credentials_are_private_distinct_and_stable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        let first = Credentials::load_or_create(&path).unwrap();
        let second = Credentials::load_or_create(&path).unwrap();
        assert_eq!(first.owner, second.owner);
        assert_eq!(first.integration, second.integration);
        assert_ne!(first.owner, first.integration);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
