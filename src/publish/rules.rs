//! Client-side publication rules.
//!
//! The registry validates an upload again, but an author should learn about a
//! refused publication before the artifact is built or the token is sent.
//! `yam-publish check` lists every rule that would fail, with the same rule
//! codes that are documented in `docs/PUBLISHING-RULES.md`.
//!
//! These checks must stay in sync with `Manager::pack` and
//! `validate_manifest`; `pack` remains the authority for the archive format.
//! Rules that can only be verified after packing (compressed size) are checked
//! by the caller.

use crate::v2::core::{ArchiveLimits, PackageSource};
use crate::v2::package::{
    expand, parse_requirement, validate_capability, validate_name, validate_payload_path,
    validate_version,
};
use crate::v2::PlatformProfile;
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

/// Maximum number of bytes scanned for secret material in one payload file.
/// A private key or a token assignment is short; skipping a larger opaque
/// binary keeps `check` from reading a whole disk image.
const MAX_SECRET_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// `description` becomes the catalogue summary on repo.yse.wtf.
const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    /// Stable rule code, for example `PKG005`.
    pub code: &'static str,
    /// Human-readable explanation naming the failing path or field.
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub version: String,
    pub targets: Vec<String>,
    /// Rule violations. A report is publishable only when this is empty.
    pub violations: Vec<Violation>,
    /// Checks that could not run, such as payload checks for a target-agnostic
    /// package without `--platform`. Never silently treated as passing.
    pub notes: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

struct Checker {
    violations: Vec<Violation>,
    seen: BTreeSet<(&'static str, String)>,
    notes: Vec<String>,
}

impl Checker {
    fn new() -> Self {
        Self {
            violations: Vec::new(),
            seen: BTreeSet::new(),
            notes: Vec::new(),
        }
    }

    fn push(&mut self, code: &'static str, message: impl Into<String>) {
        let message = message.into();
        if self.seen.insert((code, message.clone())) {
            self.violations.push(Violation { code, message });
        }
    }

    fn note(&mut self, note: impl Into<String>) {
        let note = note.into();
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// Read `yam.yaml` from `source_dir` and evaluate every rule that can be
/// checked without writing an artifact.
pub fn check(source_dir: &Path, profiles: &Path, platform: Option<&str>) -> Result<Report> {
    let descriptor = source_dir.join("yam.yaml");
    let source: PackageSource = serde_yaml::from_slice(
        &fs::read(&descriptor).with_context(|| format!("read {}", descriptor.display()))?,
    )
    .with_context(|| format!("parse {}", descriptor.display()))?;
    let mut checker = Checker::new();
    check_identity(&source, &mut checker);
    let resolved = resolve_profiles(&source, profiles, platform, &mut checker)?;
    check_requirements(&source, &mut checker);
    check_permissions(&source, &mut checker);

    if resolved.is_empty() {
        checker.note(
            "payload checks skipped: the package only declares target `any`; pass --platform",
        );
    }
    let multiple = resolved.len() > 1;
    for profile in &resolved {
        let target = artifact_target(&source, profile);
        let suffix = if multiple {
            format!(" [{}]", profile.id)
        } else {
            String::new()
        };
        check_payload(source_dir, &source, profile, &target, &suffix, &mut checker);
    }

    let mut report = Report {
        name: source.name.clone(),
        version: source.version.clone(),
        targets: source.targets.clone(),
        violations: checker.violations,
        notes: checker.notes,
    };
    report.violations.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then(left.message.cmp(&right.message))
    });
    Ok(report)
}

/// The target string `Manager::pack` will write into `MANIFEST.yaml` for this
/// profile. Kept in one place so the `any` rules describe the same artifact.
pub fn artifact_target(source: &PackageSource, profile: &PlatformProfile) -> String {
    if source.targets.iter().any(|target| target == &profile.id)
        || source
            .targets
            .iter()
            .any(|target| profile.aliases.iter().any(|alias| alias == target))
    {
        profile.id.clone()
    } else {
        "any".into()
    }
}

fn check_identity(source: &PackageSource, checker: &mut Checker) {
    if let Err(error) = validate_name(&source.name) {
        checker.push("PKG001", format!("name: {error}"));
    }
    if let Err(error) = validate_version(&source.version) {
        checker.push("PKG002", format!("version: {error}"));
    }
    if source.license.trim().is_empty() {
        checker.push(
            "PKG003",
            "license is required and must name the license of the payload",
        );
    }
    if source.description.trim().is_empty() {
        checker.push("PKG003", "description is required and must be non-empty");
    } else if source.description.chars().count() > MAX_DESCRIPTION_CHARS {
        checker.push(
            "PKG003",
            format!(
                "description is {} characters; the catalogue summary is limited to {MAX_DESCRIPTION_CHARS}",
                source.description.chars().count()
            ),
        );
    }
    if let Some(homepage) = &source.homepage {
        match url::Url::parse(homepage) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => {}
            _ => checker.push(
                "PKG003",
                format!("homepage {homepage:?} must be an absolute http(s) URL"),
            ),
        }
    }
}

fn resolve_profiles(
    source: &PackageSource,
    profiles: &Path,
    platform: Option<&str>,
    checker: &mut Checker,
) -> Result<Vec<PlatformProfile>> {
    if source.targets.is_empty() {
        checker.push("PKG004", "targets must list at least one platform or `any`");
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    let mut concrete = Vec::new();
    let mut has_any = false;
    for target in &source.targets {
        if target.is_empty() || target != target.trim() {
            checker.push(
                "PKG004",
                format!("targets contains an empty or padded value {target:?}"),
            );
            continue;
        }
        if !seen.insert(target.clone()) {
            checker.push(
                "PKG004",
                format!("targets contains duplicate value {target:?}"),
            );
        }
        if target == "any" {
            has_any = true;
        } else {
            concrete.push(target.clone());
        }
    }
    if has_any && !concrete.is_empty() {
        checker.push(
            "PKG004",
            "`any` describes a target-independent artifact and cannot be combined with concrete targets",
        );
    }
    if let Some(platform) = platform {
        let profile = PlatformProfile::load(profiles, platform)?;
        if !source
            .targets
            .iter()
            .any(|target| profile.accepts_target(target))
        {
            checker.push(
                "PKG004",
                format!(
                    "{} does not support selected platform {}; declared targets: {}",
                    source.name,
                    profile.id,
                    source.targets.join(", ")
                ),
            );
        }
        return Ok(vec![profile]);
    }
    let mut profiles_found = Vec::new();
    for target in concrete {
        match PlatformProfile::load(profiles, &target) {
            Ok(profile) => profiles_found.push(profile),
            Err(error) => checker.push(
                "PKG004",
                format!("target {target:?} has no usable platform profile: {error:#}"),
            ),
        }
    }
    Ok(profiles_found)
}

fn check_requirements(source: &PackageSource, checker: &mut Checker) {
    let lists: [(&str, &Vec<String>, bool); 6] = [
        ("depends", &source.depends, false),
        ("recommends", &source.recommends, false),
        ("enhances", &source.enhances, false),
        ("conflicts", &source.conflicts, false),
        ("replaces", &source.replaces, false),
        ("provides", &source.provides, true),
    ];
    for (label, values, capability) in lists {
        let mut seen = BTreeSet::new();
        for value in values {
            if !seen.insert(value.as_str()) {
                checker.push("PKG007", format!("{label} lists {value:?} more than once"));
                continue;
            }
            let parsed = if capability {
                validate_capability(value).map(|_| value.as_str())
            } else {
                parse_requirement(value).map(|(name, _)| name)
            };
            match parsed {
                Ok(name) => {
                    if name == source.name {
                        checker.push(
                            "PKG007",
                            format!("{label} refers to the package itself: {value:?}"),
                        );
                    }
                }
                Err(error) => checker.push(
                    "PKG007",
                    format!(
                        "{label} entry {value:?} is not a valid package requirement: {error:#}"
                    ),
                ),
            }
        }
    }
}

fn check_permissions(source: &PackageSource, checker: &mut Checker) {
    let mut seen = BTreeSet::new();
    for permission in &source.permissions {
        if permission.trim().is_empty() {
            checker.push("PKG011", "permissions must not contain an empty entry");
        } else if permission.len() > 100 || permission.chars().any(char::is_whitespace) {
            checker.push(
                "PKG011",
                format!("permission {permission:?} must be one token of at most 100 characters"),
            );
        } else if !seen.insert(permission.as_str()) {
            checker.push(
                "PKG011",
                format!("permissions lists {permission:?} more than once"),
            );
        }
    }
}

fn check_payload(
    source_dir: &Path,
    source: &PackageSource,
    profile: &PlatformProfile,
    target: &str,
    suffix: &str,
    checker: &mut Checker,
) {
    if source.files.is_empty() {
        checker.push(
            "PKG005",
            format!("files must map at least one payload path{suffix}"),
        );
        return;
    }
    let canonical_root = match source_dir.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            checker.push(
                "PKG005",
                format!(
                    "cannot resolve source directory {}: {error}",
                    source_dir.display()
                ),
            );
            return;
        }
    };
    let mut payload = BTreeMap::<String, PathBuf>::new();
    for (destination, template) in &source.files {
        if destination.contains('\\') {
            checker.push(
                "PKG005",
                format!("payload destination {destination:?} must use `/`, not `\\`{suffix}"),
            );
            continue;
        }
        if let Err(error) = validate_payload_path(destination) {
            checker.push(
                "PKG005",
                format!("payload destination {destination:?} is unsafe: {error}{suffix}"),
            );
            continue;
        }
        if Path::new(destination)
            .components()
            .any(|component| matches!(component, Component::CurDir))
        {
            checker.push(
                "PKG005",
                format!(
                    "payload destination {destination:?} must not contain `.` components{suffix}"
                ),
            );
            continue;
        }
        let expanded = expand(template, profile);
        let source_path = source_dir.join(&expanded);
        let canonical = match source_path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                checker.push(
                    "PKG005",
                    format!(
                        "payload source {template:?} (expanded to {}) is missing or unreadable: {error}{suffix}",
                        source_path.display()
                    ),
                );
                continue;
            }
        };
        if !canonical.starts_with(&canonical_root) {
            checker.push(
                "PKG005",
                format!(
                    "payload source {template:?} resolves outside the package source directory ({}){suffix}",
                    canonical.display()
                ),
            );
            continue;
        }
        if canonical.is_dir() {
            if !destination.ends_with('/') {
                checker.push(
                    "PKG005",
                    format!("directory source {template:?} requires a destination ending in `/`{suffix}"),
                );
                continue;
            }
            for entry in WalkDir::new(&canonical).sort_by_file_name() {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        checker.push(
                            "PKG005",
                            format!("cannot walk payload directory {template:?}: {error}{suffix}"),
                        );
                        continue;
                    }
                };
                if entry.file_type().is_symlink() {
                    checker.push(
                        "PKG005",
                        format!(
                            "payload symlink is not allowed: {}{suffix}",
                            entry.path().display()
                        ),
                    );
                    continue;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = match entry.path().strip_prefix(&canonical) {
                    Ok(relative) => relative,
                    Err(_) => continue,
                };
                let item = format!(
                    "{}/{}",
                    destination.trim_end_matches('/'),
                    relative.display()
                );
                if let Err(error) = validate_payload_path(&item) {
                    checker.push(
                        "PKG005",
                        format!("payload path {item:?} is unsafe: {error}{suffix}"),
                    );
                    continue;
                }
                if payload
                    .insert(item.clone(), entry.path().to_path_buf())
                    .is_some()
                {
                    checker.push("PKG005", format!("duplicate payload path {item:?}{suffix}"));
                }
            }
        } else if canonical.is_file() {
            if destination.ends_with('/') {
                checker.push(
                    "PKG005",
                    format!("destination {destination:?} ends in `/` but {template:?} is a file{suffix}"),
                );
                continue;
            }
            if payload
                .insert(destination.clone(), canonical.clone())
                .is_some()
            {
                checker.push(
                    "PKG005",
                    format!("duplicate payload path {destination:?}{suffix}"),
                );
            }
        } else {
            checker.push(
                "PKG005",
                format!(
                    "payload source {template:?} is neither a regular file nor a directory: {}{suffix}",
                    canonical.display()
                ),
            );
        }
    }

    check_share_scope(&source.name, &payload, suffix, checker);
    check_limits(&payload, suffix, checker);
    check_secret_material(&payload, suffix, checker);
    check_postinstall(source, &payload, suffix, checker);
    check_services(source, &payload, suffix, checker);
    if target == "any" {
        check_target_independent(&payload, suffix, checker);
    }
}

fn check_share_scope(
    package_name: &str,
    payload: &BTreeMap<String, PathBuf>,
    suffix: &str,
    checker: &mut Checker,
) {
    let namespace = match validate_name(package_name) {
        Ok(()) => package_name.split('/').next().unwrap_or_default(),
        Err(_) => return,
    };
    if namespace == "yam" {
        return;
    }
    let prefix = format!("{namespace}.");
    for path in payload.keys().filter(|path| path.starts_with("share/")) {
        let scoped = Path::new(path).components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|value| value.starts_with(&prefix))
        });
        if !scoped {
            checker.push(
                "PKG005",
                format!(
                    "{path} is outside yam/ and must contain a path component starting with {prefix:?}{suffix}"
                ),
            );
        }
    }
}

fn check_limits(payload: &BTreeMap<String, PathBuf>, suffix: &str, checker: &mut Checker) {
    let limits = ArchiveLimits::DEFAULT;
    if payload.len() as u64 > limits.max_entries {
        checker.push(
            "PKG008",
            format!(
                "payload has {} entries; the archive limit is {}{suffix}",
                payload.len(),
                limits.max_entries
            ),
        );
    }
    let mut total = 0u64;
    for (destination, path) in payload {
        let size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                checker.push(
                    "PKG008",
                    format!("cannot stat payload file {destination:?}: {error}{suffix}"),
                );
                continue;
            }
        };
        if size > limits.max_file_bytes {
            checker.push(
                "PKG008",
                format!(
                    "payload file {destination:?} is {size} bytes; the per-file limit is {}{suffix}",
                    limits.max_file_bytes
                ),
            );
        }
        total = total.saturating_add(size);
    }
    if total > limits.max_total_unpacked_bytes {
        checker.push(
            "PKG008",
            format!(
                "payload totals {total} bytes; the unpacked limit is {}{suffix}",
                limits.max_total_unpacked_bytes
            ),
        );
    }
}

fn check_secret_material(payload: &BTreeMap<String, PathBuf>, suffix: &str, checker: &mut Checker) {
    for (destination, path) in payload {
        if destination.starts_with(".git/") || destination.contains("/.git/") {
            checker.push(
                "PKG009",
                format!("payload must not ship Git metadata: {destination}{suffix}"),
            );
            continue;
        }
        if destination.starts_with(".ssh/") || destination.contains("/.ssh/") {
            checker.push(
                "PKG009",
                format!("payload must not ship SSH material: {destination}{suffix}"),
            );
            continue;
        }
        let size = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        if size > MAX_SECRET_SCAN_BYTES {
            continue;
        }
        let mut bytes = Vec::new();
        let read = fs::File::open(path)
            .and_then(|file| file.take(MAX_SECRET_SCAN_BYTES).read_to_end(&mut bytes));
        if read.is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (marker, what) in SECRET_MARKERS {
            if text.contains(marker) {
                checker.push(
                    "PKG009",
                    format!(
                        "payload file {destination:?} contains {what}; never publish credentials{suffix}"
                    ),
                );
            }
        }
    }
}

const SECRET_MARKERS: &[(&str, &str)] = &[
    ("-----BEGIN PRIVATE KEY-----", "a PKCS#8 private key"),
    ("-----BEGIN RSA PRIVATE KEY-----", "an RSA private key"),
    ("-----BEGIN EC PRIVATE KEY-----", "an EC private key"),
    (
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "an OpenSSH private key",
    ),
    ("YAM_PUBLISH_TOKEN=", "a publish token assignment"),
    ("ghp_", "a GitHub personal access token"),
    ("github_pat_", "a GitHub fine-grained token"),
];

fn check_postinstall(
    source: &PackageSource,
    payload: &BTreeMap<String, PathBuf>,
    suffix: &str,
    checker: &mut Checker,
) {
    let Some(executable) = source.postinstall.first() else {
        return;
    };
    if !executable.starts_with("bin/") {
        checker.push(
            "PKG010",
            format!(
                "postinstall executable must be a package file under bin/: {executable}{suffix}"
            ),
        );
    }
    if !payload.contains_key(executable) {
        checker.push(
            "PKG010",
            format!("postinstall executable is not part of the payload: {executable}{suffix}"),
        );
    }
    if source
        .postinstall
        .iter()
        .any(|argument| argument.contains('\0'))
    {
        checker.push("PKG010", format!("postinstall contains a NUL byte{suffix}"));
    }
}

fn check_services(
    source: &PackageSource,
    payload: &BTreeMap<String, PathBuf>,
    suffix: &str,
    checker: &mut Checker,
) {
    for (name, service) in &source.services {
        let exec = &service.exec;
        let env = &service.env;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            checker.push("PKG010", format!("invalid service name {name:?}{suffix}"));
        }
        let Some(executable) = exec.first() else {
            checker.push(
                "PKG010",
                format!("service {name:?} has an empty exec list{suffix}"),
            );
            continue;
        };
        if !payload.contains_key(executable) {
            checker.push(
                "PKG010",
                format!(
                    "service {name:?} executable is not part of the payload: {executable}{suffix}"
                ),
            );
        }
        if exec.iter().any(|argument| argument.contains('\0')) {
            checker.push(
                "PKG010",
                format!("service {name:?} contains a NUL argument{suffix}"),
            );
        }
        for (key, value) in env {
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
                || value.contains('\0')
            {
                checker.push(
                    "PKG010",
                    format!("service {name:?} has an invalid environment entry {key:?}{suffix}"),
                );
            }
        }
    }
}

fn check_target_independent(
    payload: &BTreeMap<String, PathBuf>,
    suffix: &str,
    checker: &mut Checker,
) {
    for (destination, path) in payload {
        let mut header = [0u8; 8];
        let read = fs::File::open(path)
            .and_then(|mut file| file.read_exact(&mut header))
            .is_ok();
        let native = read && is_native_magic(&header);
        let executable = is_executable(path);
        if native || executable {
            checker.push(
                "PKG006",
                format!(
                    "target `any` must be architecture-independent, but {destination:?} is {}; declare concrete platform targets{suffix}",
                    if native { "a native binary" } else { "marked executable" }
                ),
            );
        }
    }
}

fn is_native_magic(header: &[u8; 8]) -> bool {
    header.starts_with(b"\x7fELF")
        || header.starts_with(&[0xfe_u8, 0xed, 0xfa, 0xce])
        || header.starts_with(&[0xce_u8, 0xfa, 0xed, 0xfe])
        || header.starts_with(&[0xfe_u8, 0xed, 0xfa, 0xcf])
        || header.starts_with(&[0xcf_u8, 0xfa, 0xed, 0xfe])
        || header.starts_with(&[0xca_u8, 0xfe, 0xba, 0xbe])
        || header.starts_with(b"MZ")
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::PlatformProfile;
    use std::fs;
    use tempfile::TempDir;

    fn profile(dir: &Path, id: &str, aliases: &[&str]) {
        let aliases = aliases
            .iter()
            .map(|alias| format!("  - {alias}\n"))
            .collect::<String>();
        let profiles = dir.join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        fs::write(
            profiles.join(format!("{id}.yaml")),
            format!(
                "id: {id}\n{}arch: aarch64\ntriple: aarch64-unknown-linux-musl\npaths:\n  root: /tmp/yam-test\n  etc: /tmp/yam-etc\n  var: /tmp/yam-var\n  run: /tmp/yam-run\n",
                if aliases.is_empty() {
                    String::new()
                } else {
                    format!("aliases:\n{aliases}")
                }
            ),
        )
        .unwrap();
    }

    fn source(dir: &Path, yaml: &str) {
        fs::write(dir.join("yam.yaml"), yaml).unwrap();
    }

    fn check_dir(dir: &TempDir, platform: Option<&str>) -> Report {
        check(
            &dir.path().join("src"),
            &dir.path().join("profiles"),
            platform,
        )
        .unwrap()
    }

    fn codes(report: &Report) -> Vec<&str> {
        report
            .violations
            .iter()
            .map(|violation| violation.code)
            .collect()
    }

    fn valid_tree() -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src/share/author.player")).unwrap();
        fs::create_dir_all(dir.path().join("profiles")).unwrap();
        fs::write(
            dir.path().join("src/share/author.player/readme.txt"),
            "hello",
        )
        .unwrap();
        profile(&dir.path(), "testboard", &[]);
        dir
    }

    #[test]
    fn a_clean_package_passes() {
        let dir = valid_tree();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(report.ok(), "{:?}", report.violations);
    }

    #[test]
    fn invalid_name_version_and_metadata_are_reported() {
        let dir = valid_tree();
        source(
            &dir.path().join("src"),
            "name: Author/Player\nversion: 1.0\nlicense: \"\"\ndescription: \"\"\ntargets: [testboard]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(codes(&report).contains(&"PKG001"));
        assert!(codes(&report).contains(&"PKG002"));
        assert_eq!(
            report
                .violations
                .iter()
                .filter(|violation| violation.code == "PKG003")
                .count(),
            2
        );
    }

    #[test]
    fn unresolvable_target_is_reported() {
        let dir = valid_tree();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [nosuchboard]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(codes(&report).contains(&"PKG004"));
    }

    #[test]
    fn unscoped_share_path_is_reported_for_foreign_namespaces() {
        let dir = valid_tree();
        fs::write(dir.path().join("src/share/unscoped.txt"), "hello").unwrap();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard]\nfiles:\n  share/unscoped.txt: share/unscoped.txt\n",
        );
        let report = check_dir(&dir, None);
        assert!(report
            .violations
            .iter()
            .any(|violation| violation.code == "PKG005"
                && violation.message.contains("share/unscoped.txt")));
    }

    #[test]
    fn source_outside_the_package_directory_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("profiles")).unwrap();
        fs::write(dir.path().join("secret.bin"), "secret").unwrap();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard]\nfiles:\n  bin/player: ../secret.bin\n",
        );
        profile(&dir.path(), "testboard", &[]);
        let report = check_dir(&dir, None);
        assert!(report.violations.iter().any(|violation| violation
            .message
            .contains("outside the package source directory")));
    }

    #[test]
    fn private_key_material_is_rejected() {
        let dir = valid_tree();
        fs::write(
            dir.path().join("src/share/author.player/key.txt"),
            "-----BEGIN PRIVATE KEY-----\nMIIE\n",
        )
        .unwrap();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(codes(&report).contains(&"PKG009"));
    }

    #[cfg(unix)]
    #[test]
    fn target_any_rejects_native_payloads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = valid_tree();
        fs::create_dir_all(dir.path().join("src/bin")).unwrap();
        let binary = dir.path().join("src/bin/player");
        fs::write(&binary, b"\x7fELF\x02\x01\x01").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [any]\nfiles:\n  bin/player: bin/player\n",
        );
        let report = check_dir(&dir, Some("testboard"));
        assert!(codes(&report).contains(&"PKG006"));
    }

    #[test]
    fn self_dependency_is_rejected() {
        let dir = valid_tree();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard]\ndepends: [author/player >=1.0.0]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(codes(&report).contains(&"PKG007"));
    }

    #[test]
    fn payload_entries_are_counted_per_declared_target() {
        let dir = valid_tree();
        profile(&dir.path(), "otherboard", &[]);
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [testboard, otherboard]\nfiles:\n  share/author.player/: share/author.player/\n",
        );
        let report = check_dir(&dir, None);
        assert!(report.ok(), "{:?}", report.violations);
    }

    #[test]
    fn missing_platform_for_target_any_is_reported_as_a_note() {
        let dir = valid_tree();
        source(
            &dir.path().join("src"),
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [any]\nfiles:\n  share/author.player/readme.txt: share/author.player/readme.txt\n",
        );
        let report = check_dir(&dir, None);
        assert!(report.ok());
        assert!(!report.notes.is_empty());
    }

    #[test]
    fn artifact_target_matches_pack_rules() {
        let dir = valid_tree();
        profile(&dir.path(), "newboard", &["aliasboard"]);
        let source: PackageSource = serde_yaml::from_str(
            "name: author/player\nversion: 1.0.0\nlicense: MIT\ndescription: Player\ntargets: [aliasboard]\nfiles:\n  bin/x: bin/x\n",
        )
        .unwrap();
        let profile = PlatformProfile::load(&dir.path().join("profiles"), "newboard").unwrap();
        assert_eq!(artifact_target(&source, &profile), "newboard");
    }
}
