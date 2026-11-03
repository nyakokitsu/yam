use anyhow::{bail, Context, Result};
use fs4::fs_std::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    process::Command,
};
use tar::{Builder, Header};
use uuid::Uuid;

use super::package::*;
use super::registry;
use super::version;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformProfile {
    pub id: String,
    /// Registry targets that publish artifacts for this platform under a
    /// different id (the Midi and Light 2 codenames).
    #[serde(default)]
    pub aliases: Vec<String>,
    pub arch: String,
    pub triple: String,
    pub paths: PlatformPaths,
    #[serde(default)]
    pub firmware_version_file: Option<PathBuf>,
    #[serde(default)]
    pub init: Option<String>,
    #[serde(default)]
    pub boot_hook: Option<PathBuf>,
    #[serde(default)]
    pub entware: Option<EntwareProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformPaths {
    pub root: PathBuf,
    pub etc: PathBuf,
    pub var: PathBuf,
    pub run: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntwareProfile {
    pub opkg: PathBuf,
}

impl PlatformProfile {
    pub fn load(directory: &Path, id: &str) -> Result<Self> {
        validate_platform_id(id)?;
        let path = directory.join(format!("{id}.yaml"));
        let profile: Self = serde_yaml::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )?;
        if profile.id != id {
            bail!(
                "profile {} has mismatched id {}",
                path.display(),
                profile.id
            )
        }
        validate_platform_id(&profile.id)?;
        for alias in &profile.aliases {
            validate_platform_id(alias)
                .with_context(|| format!("profile {} has invalid alias {alias}", path.display()))?;
        }
        Ok(profile)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSource {
    pub name: String,
    pub version: String,
    pub license: String,
    pub description: String,
    #[serde(default)]
    pub homepage: Option<String>,
    pub targets: Vec<String>,
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub recommends: Vec<String>,
    #[serde(default)]
    pub enhances: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub entware: Option<EntwareRequest>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSpec>,
    /// Command run after this package's payload and links are staged.
    /// The first item is a `bin/` file from this package; remaining items
    /// are literal arguments, never interpreted by a shell.
    #[serde(default)]
    pub postinstall: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntwareRequest {
    pub packages: Vec<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub exec: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub oneshot: bool,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    #[default]
    Never,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    pub license: String,
    pub description: String,
    #[serde(default)]
    pub homepage: Option<String>,
    pub target: String,
    pub arch: String,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub recommends: Vec<String>,
    #[serde(default)]
    pub enhances: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub entware: Option<EntwareRequest>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSpec>,
    #[serde(default)]
    pub postinstall: Vec<String>,
    pub files_sha256: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct InstalledService {
    pub package: String,
    pub name: String,
    pub enabled: bool,
    pub spec: ServiceSpec,
}

pub struct Manager {
    root: PathBuf,
    profile: PlatformProfile,
    db: Connection,
    /// Set when `db/transaction.json` exists at open time: a previous
    /// mutating command was interrupted (most likely killed) before it
    /// could finish its rollback or its cleanup. Mutating commands
    /// refuse to run until `repair` clears it.
    interrupted: bool,
    /// Test-only fault injection for B1's rollback path: when set, the
    /// Nth link created during `install_payload` fails instead of
    /// succeeding, so a test can assert the disk and database end up
    /// exactly as they were before the call. Never present in a release
    /// build; never settable from an environment variable or any other
    /// production surface.
    #[cfg(test)]
    fail_after_links: Option<u32>,
    /// Test-only fault injection for `remove`'s rollback path: when set,
    /// removing the Nth link fails instead of succeeding.
    #[cfg(test)]
    fail_after_link_removals: Option<u32>,
    /// Test-only override of the archive extraction limits (B4), so a
    /// test can exceed a tiny limit without building a multi-megabyte
    /// archive.
    #[cfg(test)]
    archive_limits_override: Option<ArchiveLimits>,
}

/// Hard ceilings enforced while extracting a `.yampkg`, checked against
/// bytes actually read rather than any size a tar header claims (B4): a
/// small compressed artifact can still decompress to something huge.
#[derive(Debug, Clone)]
pub(crate) struct ArchiveLimits {
    pub(crate) max_compressed_bytes: u64,
    pub(crate) max_total_unpacked_bytes: u64,
    pub(crate) max_file_bytes: u64,
    pub(crate) max_entries: u64,
}
impl ArchiveLimits {
    const DEFAULT: Self = Self {
        max_compressed_bytes: 64 * 1024 * 1024,
        max_total_unpacked_bytes: 256 * 1024 * 1024,
        max_file_bytes: 64 * 1024 * 1024,
        max_entries: 10_000,
    };
}

include!("manager.rs");

/// One already-applied disk change from `install_payload`'s disk phase,
/// recorded so it can be undone in reverse if a later step in that same
/// phase fails (B1). Database writes need no equivalent: they live in an
/// uncommitted `rusqlite::Transaction` that rolls back on drop.
enum Undo {
    /// A directory `install_payload` created from nothing (the new
    /// version's payload directory): delete it entirely.
    RemoveDirAll(PathBuf),
    /// A symlink that replaced an existing one: put the old target back.
    RestoreSymlink { path: PathBuf, target: PathBuf },
    /// A symlink created where nothing existed before: remove it.
    RemoveSymlinkFile(PathBuf),
}
impl Undo {
    fn apply(self) -> std::result::Result<(), String> {
        match self {
            Undo::RemoveDirAll(path) => fs::remove_dir_all(&path)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| format!("remove {}: {error}", path.display())),
            Undo::RemoveSymlinkFile(path) => fs::remove_file(&path)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|error| format!("remove {}: {error}", path.display())),
            Undo::RestoreSymlink { path, target } => {
                if path.is_symlink() || path.exists() {
                    let _ = fs::remove_file(&path);
                }
                make_link(&path, &target)
                    .map_err(|error| format!("restore {}: {error}", path.display()))
            }
        }
    }
}
/// Undo every recorded disk change, most recent first, continuing past
/// individual failures so a single stuck step doesn't leave the rest of
/// the rollback undone. Returns a description of each step that failed
/// to undo (empty if the disk is back to exactly how it was).
fn unwind(actions: Vec<Undo>) -> Vec<String> {
    actions
        .into_iter()
        .rev()
        .filter_map(|action| action.apply().err())
        .collect()
}
fn append_bytes<W: Write>(archive: &mut Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, path, Cursor::new(bytes))?;
    Ok(())
}
#[cfg(unix)]
fn make_link(link: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}
#[cfg(not(unix))]
fn make_link(_: &Path, _: &Path) -> Result<()> {
    bail!("v2 package links require a Unix host")
}
fn replace_link(path: &Path, target: &Path) -> Result<()> {
    let temporary = path.with_extension(format!("new-{}", Uuid::new_v4()));
    make_link(&temporary, target)?;
    fs::rename(temporary, path)?;
    Ok(())
}
pub(crate) fn set_payload_mode(path: &Path, payload_path: &str, archive_mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Preserve only the regular file's executable intent, never arbitrary
        // archive permissions (setuid/setgid/sticky or group/world write).
        // Keep the existing bin/hooks default for older authoring workflows.
        let mode = if archive_mode & 0o111 != 0
            || payload_path.starts_with("bin/")
            || payload_path.starts_with("hooks/")
        {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;
