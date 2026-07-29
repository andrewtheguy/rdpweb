use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Parser;
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(about = "Minimal RDP-to-WebCodecs AVC420 passthrough gateway")]
pub struct Args {
    /// remotex-compatible TOML file containing one or more [[targets]].
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Target name to choose from --config. The first RDP target is used if omitted.
    #[arg(long)]
    pub target: Option<String>,

    /// HTTP listen address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// RDP host. Used when --config is omitted.
    #[arg(long, env = "RDPWEB_HOST")]
    pub host: Option<String>,

    /// RDP port. Used when --config is omitted.
    #[arg(long, env = "RDPWEB_PORT", default_value_t = 3389)]
    pub port: u16,

    /// RDP username. Used when --config is omitted.
    #[arg(long, env = "RDPWEB_USERNAME")]
    pub username: Option<String>,

    /// RDP password. Prefer the RDPWEB_PASSWORD environment variable.
    #[arg(long, env = "RDPWEB_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// Requested desktop width. Used when --config is omitted.
    #[arg(long, env = "RDPWEB_WIDTH", default_value_t = 1280)]
    pub width: u16,

    /// Requested desktop height. Used when --config is omitted.
    #[arg(long, env = "RDPWEB_HEIGHT", default_value_t = 800)]
    pub height: u16,
}

#[derive(Clone, Deserialize)]
pub struct Target {
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(default = "default_width")]
    pub width: u16,
    #[serde(default = "default_height")]
    pub height: u16,
}

#[derive(Deserialize)]
struct FileConfig {
    targets: Vec<Target>,
}

fn default_port() -> u16 {
    3389
}

fn default_width() -> u16 {
    1280
}

fn default_height() -> u16 {
    800
}

impl Args {
    pub fn resolve_target(self) -> anyhow::Result<Target> {
        if let Some(path) = self.config {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read config {}", path.display()))?;
            let config: FileConfig =
                toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;

            let selected = match self.target {
                Some(name) => config
                    .targets
                    .into_iter()
                    .find(|target| target.name.as_deref() == Some(&name))
                    .with_context(|| format!("target {name:?} not found in {}", path.display()))?,
                None => config
                    .targets
                    .into_iter()
                    .find(|target| target.protocol.as_deref().unwrap_or("rdp") == "rdp")
                    .with_context(|| format!("no RDP target found in {}", path.display()))?,
            };

            if selected.protocol.as_deref().unwrap_or("rdp") != "rdp" {
                bail!("selected target is not an RDP target");
            }
            return validate(selected);
        }

        let host = self
            .host
            .context("--host or RDPWEB_HOST is required when --config is omitted")?;
        let username = self
            .username
            .context("--username or RDPWEB_USERNAME is required when --config is omitted")?;
        let password = self
            .password
            .context("--password or RDPWEB_PASSWORD is required when --config is omitted")?;

        validate(Target {
            protocol: Some("rdp".to_owned()),
            name: None,
            host,
            port: self.port,
            username,
            password,
            width: self.width,
            height: self.height,
        })
    }
}

fn validate(target: Target) -> anyhow::Result<Target> {
    if target.host.trim().is_empty() {
        bail!("RDP host is empty");
    }
    if target.username.is_empty() {
        bail!("RDP username is empty");
    }
    if target.width == 0 || target.height == 0 {
        bail!("RDP desktop size must be non-zero");
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remotex_target_shape() {
        let config: FileConfig = toml::from_str(
            r#"
                [[targets]]
                protocol = "rdp"
                name = "desktop"
                host = "2001:db8::20"
                port = 3389
                audio = true
                username = "user"
                password = "secret"
                width = 1280
                height = 800
                resize = true
                clipboard = true
            "#,
        )
        .expect("parse config");

        let target = &config.targets[0];
        assert_eq!(target.name.as_deref(), Some("desktop"));
        assert_eq!(target.host, "2001:db8::20");
        assert_eq!((target.width, target.height), (1280, 800));
    }
}
