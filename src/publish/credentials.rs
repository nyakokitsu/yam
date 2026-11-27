//! Local credentials for publishing.
//!
//! A publish token and the namespace key are stored per registry name, outside
//! the repository and outside any package source tree:
//!
//! * `YAM_CONFIG_DIR` when set (also what CI should use);
//! * `%APPDATA%\yam\credentials.json` on Windows;
//! * `$XDG_CONFIG_HOME/yam/credentials.json`, else
//!   `$HOME/.config/yam/credentials.json` on Unix.
//!
//! The file is written `0600` via a temporary file and an atomic rename. The
//! token is never printed; `auth status` shows a SHA-256 fingerprint instead.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "format_version")]
    pub format: u32,
    #[serde(default = "default_registry")]
    pub default: String,
    #[serde(default)]
    pub registries: BTreeMap<String, StoredRegistry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredRegistry {
    pub upload_url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// Login reported by the registry's `whoami` endpoint during `auth login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

fn format_version() -> u32 {
    FORMAT
}

fn default_registry() -> String {
    "official".into()
}

impl Store {
    pub fn load() -> Result<Self> {
        Self::load_from(&default_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    format: FORMAT,
                    default: default_registry(),
                    registries: BTreeMap::new(),
                })
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read credentials {}", path.display()))
            }
        };
        let store: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse credentials {}", path.display()))?;
        if store.format != FORMAT {
            bail!(
                "credentials {} use format {}; this yam understands format {FORMAT}",
                path.display(),
                store.format
            );
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&default_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("credentials path has no parent directory")?;
        create_private_dir(parent)?;
        let temporary = parent.join(format!("credentials-{}.tmp", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec_pretty(self)?;
        write_private_file(&temporary, &bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        replace_file(&temporary, path)
            .with_context(|| format!("replace credentials {}", path.display()))?;
        Ok(())
    }

    pub fn entry(&self, registry: &str) -> Option<&StoredRegistry> {
        self.registries.get(registry)
    }

    pub fn insert(&mut self, registry: &str, entry: StoredRegistry) {
        self.registries.insert(registry.to_string(), entry);
    }

    pub fn remove(&mut self, registry: &str) -> bool {
        self.registries.remove(registry).is_some()
    }
}

/// Resolved values for one publish command.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub registry: String,
    pub upload_url: String,
    pub token: String,
    pub key: PathBuf,
}

/// Precedence for every value: explicit flag/environment, then the stored
/// entry for `registry`, then the built-in default for the upload URL.
pub fn resolve(
    registry: &str,
    upload_url: Option<&str>,
    token: Option<&str>,
    key: Option<&Path>,
) -> Result<Resolved> {
    resolve_with(&Store::load()?, registry, upload_url, token, key)
}

pub fn resolve_with(
    store: &Store,
    registry: &str,
    upload_url: Option<&str>,
    token: Option<&str>,
    key: Option<&Path>,
) -> Result<Resolved> {
    let entry = store.entry(registry);
    let upload_url = upload_url
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| entry.map(|entry| entry.upload_url.clone()))
        .unwrap_or_else(|| crate::publish::client::DEFAULT_UPLOAD_URL.into());
    let token = token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .map(|entry| entry.token.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .with_context(|| {
            format!(
                "publish token is required for registry {registry:?}: pass --token, set YAM_PUBLISH_TOKEN, or run `yam auth login`"
            )
        })?;
    let key = key
        .map(Path::to_path_buf)
        .or_else(|| entry.and_then(|entry| entry.key_file.clone()))
        .with_context(|| {
            format!(
                "author key is required for registry {registry:?}: pass --key, set YAM_AUTHOR_KEY_FILE, or run `yam auth login --key <file>`"
            )
        })?;
    if !key.is_file() {
        bail!("author key is not a file: {}", key.display())
    }
    Ok(Resolved {
        registry: registry.to_string(),
        upload_url,
        token,
        key,
    })
}

/// Cache of values that do not need the key, used by `auth status` and
/// `status`, which must work even if the private key file has moved.
pub fn resolve_token_only(
    registry: &str,
    upload_url: Option<&str>,
    token: Option<&str>,
) -> Result<(String, String, Option<StoredRegistry>)> {
    let store = Store::load()?;
    let entry = store.entry(registry).cloned();
    let upload_url = upload_url
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| entry.as_ref().map(|entry| entry.upload_url.clone()))
        .unwrap_or_else(|| crate::publish::client::DEFAULT_UPLOAD_URL.into());
    let token = token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .as_ref()
                .map(|entry| entry.token.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .with_context(|| {
            format!(
                "publish token is required for registry {registry:?}: pass --token, set YAM_PUBLISH_TOKEN, or run `yam auth login`"
            )
        })?;
    Ok((upload_url, token, entry))
}

/// Short, non-reversible fingerprint for display. Never show the token itself.
pub fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    format!("sha256:{}", hex(&digest[..8]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("YAM_CONFIG_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return Ok(PathBuf::from(appdata).join("yam"));
        }
    }
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        if !config.is_empty() {
            return Ok(PathBuf::from(config).join("yam"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config").join("yam"));
    }
    bail!("cannot determine a config directory; set YAM_CONFIG_DIR")
}

pub fn default_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials.json"))
}

/// Copy a PEM key into the config directory so `auth login` survives moving or
/// deleting the original download. Never called implicitly.
pub fn copy_key(source: &Path, registry: &str) -> Result<PathBuf> {
    if registry.is_empty()
        || !registry
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid registry name {registry:?}")
    }
    let keys = config_dir()?.join("keys");
    create_private_dir(&keys)?;
    let destination = keys.join(format!("{registry}-{}.pem", uuid::Uuid::new_v4()));
    let bytes = fs::read(source).with_context(|| format!("read {}", source.display()))?;
    write_private_file(&destination, &bytes)
        .with_context(|| format!("write {}", destination.display()))?;
    Ok(destination)
}

/// Only remove keys created by YAM, never an externally supplied path.
pub fn remove_obsolete_key(registry: &str, entry: &StoredRegistry, store: &Store) -> Result<()> {
    let Some(path) = entry.key_file.as_ref() else {
        return Ok(());
    };
    if path.parent() != Some(config_dir()?.join("keys").as_path())
        || store
            .registries
            .values()
            .any(|e| e.key_file.as_ref() == Some(path))
    {
        return Ok(());
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    let managed = stem == registry
        || stem
            .strip_prefix(&format!("{registry}-"))
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok());
    if managed && path.extension().is_some_and(|x| x == "pem") {
        match fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

/// `fs::rename` replaces an existing file on Unix but fails on Windows.
fn replace_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) if cfg!(windows) && destination.exists() => {
            fs::remove_file(destination)?;
            fs::rename(temporary, destination).map_err(|_| error)
        }
        Err(error) => Err(error),
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o700);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("restrict {}", path.display()))?;
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> Store {
        Store {
            format: FORMAT,
            default: "official".into(),
            registries: BTreeMap::from([(
                "official".into(),
                StoredRegistry {
                    upload_url: "https://registry.example/api/v2/upload".into(),
                    token: "secret-token".into(),
                    key_file: Some(PathBuf::from("/keys/author.pem")),
                    login: Some("author".into()),
                },
            )]),
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = TempDir::new().unwrap();
        let store = Store::load_from(&dir.path().join("missing.json")).unwrap();
        assert!(store.registries.is_empty());
        assert_eq!(store.default, "official");
    }

    #[test]
    fn saved_credentials_roundtrip_and_are_private() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/credentials.json");
        store().save_to(&path).unwrap();
        let loaded = Store::load_from(&path).unwrap();
        assert_eq!(loaded.registries["official"].token, "secret-token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn explicit_values_win_over_stored_values() {
        let stored = store();
        let resolved = resolve_with(
            &stored,
            "official",
            Some("https://other.example/api/v2/upload"),
            Some("explicit-token"),
            Some(Path::new("Cargo.toml")),
        )
        .unwrap();
        assert_eq!(resolved.upload_url, "https://other.example/api/v2/upload");
        assert_eq!(resolved.token, "explicit-token");
        assert_eq!(resolved.key, Path::new("Cargo.toml"));
    }

    #[test]
    fn stored_values_fill_the_gaps() {
        let stored = store();
        let error = format!(
            "{:#}",
            resolve_with(&stored, "official", None, Some("inline"), None).unwrap_err()
        );
        // The stored key is intentionally a path outside the test tree, so
        // resolution must fail with an actionable message instead of using it.
        assert!(error.contains("/keys/author.pem"), "{error}");

        let resolved = resolve_with(
            &stored,
            "official",
            None,
            Some("inline"),
            Some(Path::new("Cargo.toml")),
        )
        .unwrap();
        assert_eq!(resolved.token, "inline");
        assert_eq!(
            resolved.upload_url,
            "https://registry.example/api/v2/upload"
        );
    }

    #[test]
    fn missing_token_names_the_login_command() {
        let empty = Store {
            format: FORMAT,
            default: "official".into(),
            registries: BTreeMap::new(),
        };
        let error = format!(
            "{:#}",
            resolve_with(
                &empty,
                "official",
                None,
                None,
                Some(Path::new("Cargo.toml"))
            )
            .unwrap_err()
        );
        assert!(error.contains("auth login"), "{error}");
    }

    #[test]
    fn fingerprints_are_short_and_do_not_reveal_the_token() {
        let fingerprint = fingerprint("secret-token");
        assert!(fingerprint.starts_with("sha256:"));
        assert_eq!(fingerprint.len(), "sha256:".len() + 16);
        assert!(!fingerprint.contains("secret"));
    }
}
