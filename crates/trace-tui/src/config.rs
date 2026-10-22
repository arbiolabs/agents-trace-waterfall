use anyhow::Context;
use serde::Deserialize;
use std::path::PathBuf;
use trace_render::{Glyphs, Palette, Theme};

/// `~/.config/trace-waterfall/config.toml`, or `$TRACE_WATERFALL_CONFIG`. A missing file means defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub db: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub theme: ThemeName,
    pub retention: Retention,
    pub tail: Vec<Tail>,
    pub capture: trace_ingest::Capture,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    Default,
    Ascii,
    Monochrome,
    AsciiMonochrome,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Retention {
    /// Sessions (events and projections) older than this are purged. None keeps forever.
    pub metadata_days: Option<u32>,
    pub payload_days: Option<u32>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            metadata_days: Some(90),
            payload_days: Some(30),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tail {
    pub adapter: String,
    pub path: PathBuf,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var_os("TRACE_WATERFALL_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config/trace-waterfall/config.toml"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut c: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        c.db = c.db.map(expand_home);
        c.socket = c.socket.map(expand_home);
        for t in &mut c.tail {
            t.path = expand_home(std::mem::take(&mut t.path));
        }
        Ok(c)
    }

    /// `$TRACE_WATERFALL_DB`, then config, then the storage default.
    pub fn db_path(&self) -> PathBuf {
        env_path("TRACE_WATERFALL_DB")
            .or_else(|| self.db.clone())
            .unwrap_or_else(trace_storage::default_db_path)
    }

    pub fn socket_path(&self) -> PathBuf {
        env_path("TRACE_WATERFALL_SOCKET")
            .or_else(|| self.socket.clone())
            .unwrap_or_else(trace_adapter_sdk::default_socket_path)
    }

    pub fn theme(&self) -> Theme {
        let (glyphs, palette) = match self.theme {
            ThemeName::Default => (Glyphs::Unicode, Palette::Color),
            ThemeName::Ascii => (Glyphs::Ascii, Palette::Color),
            ThemeName::Monochrome => (Glyphs::Unicode, Palette::Mono),
            ThemeName::AsciiMonochrome => (Glyphs::Ascii, Palette::Mono),
        };
        Theme { glyphs, palette }
    }
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn expand_home(p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p,
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_leading_tilde_only() {
        assert_eq!(expand_home("~/x".into()), home().join("x"));
        assert_eq!(expand_home("/a/~/x".into()), PathBuf::from("/a/~/x"));
    }

    #[test]
    fn parses_full_file() {
        let c: Config = toml::from_str(
            r#"
            db = "/tmp/t.db"
            theme = "ascii-monochrome"
            [retention]
            metadata_days = 7
            [capture]
            payloads = false
            projects = ["p"]
            [[tail]]
            adapter = "claude-code"
            path = "/x"
            "#,
        )
        .unwrap();
        assert_eq!(c.db, Some(PathBuf::from("/tmp/t.db")));
        assert_eq!((c.capture.payloads, c.capture.projects.as_slice()), (false, ["p".to_string()].as_slice()));
        assert_eq!(
            c.theme(),
            Theme {
                glyphs: Glyphs::Ascii,
                palette: Palette::Mono
            }
        );
        assert_eq!(c.retention.metadata_days, Some(7));
        assert_eq!(c.retention.payload_days, Some(30));
        assert_eq!(c.tail[0].adapter, "claude-code");
        let d: Config = toml::from_str("").unwrap();
        assert_eq!(d.retention.metadata_days, Some(90));
    }
}
