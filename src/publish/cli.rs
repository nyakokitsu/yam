//! Command-line surface shared by `yam-publish` and the desktop `yam` binary.
//!
//! Both binaries expose the same publishing commands. Keeping argument parsing
//! and command bodies here avoids a second, drifting implementation and lets
//! the station binary stay free of publishing code paths people never call.

use crate::publish::{client, credentials, rules, sign};
use crate::v2::{Manager, PlatformProfile};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Args, Clone)]
pub struct CheckArgs {
    /// Directory containing yam.yaml.
    #[arg(long, default_value = ".")]
    pub source: PathBuf,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    pub profiles: PathBuf,
    /// Platform ID from the profiles directory. Defaults to the concrete
    /// target in yam.yaml when it declares exactly one.
    #[arg(long, env = "YAM_PLATFORM")]
    pub platform: Option<String>,
    /// Print a machine-readable report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct PackArgs {
    /// Directory containing yam.yaml.
    #[arg(long, default_value = ".")]
    pub source: PathBuf,
    /// Platform ID from the profiles directory. Defaults to YAM_PLATFORM,
    /// then to the sole concrete target in yam.yaml.
    #[arg(long, env = "YAM_PLATFORM")]
    pub platform: Option<String>,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    pub profiles: PathBuf,
    /// Destination .yampkg path.
    #[arg(short, long)]
    pub output: PathBuf,
}

#[derive(Args, Clone)]
pub struct PublishArgs {
    /// Built .yampkg archive.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Directory containing the source yam.yaml used to create the artifact.
    #[arg(long, default_value = ".")]
    pub source: PathBuf,
    /// Target used for this artifact.
    #[arg(long)]
    pub target: String,
    /// Stored credentials entry to use; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL")]
    pub upload_url: Option<String>,
    /// Publish token created in the registry profile. Falls back to
    /// YAM_PUBLISH_TOKEN, then to `yam-publish auth login`.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    pub token: Option<String>,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    /// Falls back to YAM_AUTHOR_KEY_FILE, then to the stored key.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    pub key: Option<PathBuf>,
    /// Follow the moderation queue until the artifact is approved or rejected.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Args, Clone)]
pub struct ReleaseArgs {
    /// Package source directory containing yam.yaml. Builds every declared
    /// concrete target unless --platform is given.
    #[arg(long, default_value = ".")]
    pub source: PathBuf,
    /// Build and publish only this platform instead of every declared target.
    #[arg(long, env = "YAM_PLATFORM")]
    pub platform: Option<String>,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    pub profiles: PathBuf,
    /// Directory for built artifacts. Defaults to <source>/dist.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Write this artifact path; only valid with --platform or a single target.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Build and validate every artifact, but do not upload.
    #[arg(long)]
    pub dry_run: bool,
    /// Stored credentials entry to use; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL")]
    pub upload_url: Option<String>,
    /// Publish token created in the registry profile. Falls back to
    /// YAM_PUBLISH_TOKEN, then to `yam-publish auth login`.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    pub token: Option<String>,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    /// Falls back to YAM_AUTHOR_KEY_FILE, then to the stored key.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    pub key: Option<PathBuf>,
    /// Follow every moderation submission until it is approved or rejected.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Args, Clone)]
pub struct StatusArgs {
    /// Submission id returned by `publish`.
    pub id: String,
    /// Stored credentials entry to use; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL")]
    pub upload_url: Option<String>,
    /// Publish token created in the registry profile. Falls back to
    /// YAM_PUBLISH_TOKEN, then to `yam-publish auth login`.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    pub token: Option<String>,
    /// Keep polling until the submission is approved or rejected.
    #[arg(long)]
    pub watch: bool,
    /// Seconds between status requests while watching.
    #[arg(long, default_value_t = 5)]
    pub interval: u64,
    /// Give up after this many seconds; 0 waits forever.
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    /// Print one JSON object per state instead of human-readable lines.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand, Clone)]
pub enum AuthCommand {
    /// Save a publish token and author key for a registry.
    Login(AuthLoginArgs),
    /// Show the stored credentials for a registry.
    Status(AuthStatusArgs),
    /// Remove the stored credentials for a registry.
    Logout(AuthLogoutArgs),
}

#[derive(Args, Clone)]
pub struct AuthLoginArgs {
    /// Stored credentials entry; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL")]
    pub upload_url: Option<String>,
    /// Publish token. Prefer YAM_PUBLISH_TOKEN in CI; without it the token is
    /// read from stdin.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    pub token: Option<String>,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    pub key: Option<PathBuf>,
    /// Copy the key into the config directory instead of remembering its path.
    #[arg(long)]
    pub store_key: bool,
    /// Save without calling the registry account endpoint.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Args, Clone)]
pub struct AuthStatusArgs {
    /// Stored credentials entry; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL")]
    pub upload_url: Option<String>,
    /// Ask the registry to verify the token and show the account.
    #[arg(long)]
    pub verify: bool,
    /// Print one JSON object instead of human-readable lines.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Clone)]
pub struct AuthLogoutArgs {
    /// Stored credentials entry; defaults to `official`.
    #[arg(long, default_value = "official")]
    pub registry: String,
    /// Remove every stored registry.
    #[arg(long)]
    pub all: bool,
}

#[derive(Deserialize)]
struct SourceManifest {
    name: String,
    version: String,
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Debug)]
struct TargetPlan {
    profile: PlatformProfile,
    target: String,
}

pub fn check(args: &CheckArgs) -> Result<()> {
    let report = rules::check(&args.source, &args.profiles, args.platform.as_deref())?;
    if args.json {
        print_report_json(&report);
    } else {
        print_report(&report);
    }
    if !report.ok() {
        bail!(
            "publication refused: {} rule violation(s); see docs/PUBLISHING-RULES.md",
            report.violations.len()
        )
    }
    Ok(())
}

pub fn pack(args: &PackArgs) -> Result<()> {
    let report = rules::check(&args.source, &args.profiles, args.platform.as_deref())?;
    print_report(&report);
    if !report.ok() {
        bail!(
            "publication refused: {} rule violation(s); see docs/PUBLISHING-RULES.md",
            report.violations.len()
        )
    }
    let profile = resolve_platform(&args.source, &args.profiles, args.platform.as_deref())?;
    let artifact = pack_artifact(&args.source, &profile, &args.output)?;
    println!(
        "Packed {} {} for {}: {} ({}).",
        report.name,
        report.version,
        profile.id,
        artifact.display(),
        human_size(fs::metadata(&artifact)?.len())
    );
    Ok(())
}

pub fn publish(args: &PublishArgs) -> Result<()> {
    let manifest = read_manifest(&args.source)?;
    let artifact =
        fs::read(&args.artifact).with_context(|| format!("read {}", args.artifact.display()))?;
    if artifact.is_empty() {
        bail!("artifact is empty")
    }
    let resolved = credentials::resolve(
        &args.registry,
        args.upload_url.as_deref(),
        args.token.as_deref(),
        args.key.as_deref(),
    )?;
    let sha256 = format!("{:x}", Sha256::digest(&artifact));
    let submission = upload(
        &resolved.upload_url,
        &resolved.token,
        &resolved.key,
        &manifest,
        &args.target,
        &args.artifact,
        &artifact,
        &sha256,
    )?;
    match submission {
        Some(submission) => {
            println!("Check it with `yam status {} --watch`.", submission.id);
            if args.wait {
                follow(
                    &resolved.upload_url,
                    &resolved.token,
                    &submission.id,
                    5,
                    900,
                    false,
                )?;
            }
        }
        None => {
            println!(
                "Published {} {} for {}.",
                manifest.name, manifest.version, args.target
            );
            if args.wait {
                eprintln!(
                    "note: this registry does not report moderation submissions; nothing to wait for"
                );
            }
        }
    }
    Ok(())
}

pub fn status(args: &StatusArgs) -> Result<()> {
    let (upload_url, token, _) = credentials::resolve_token_only(
        &args.registry,
        args.upload_url.as_deref(),
        args.token.as_deref(),
    )?;
    if args.watch {
        follow(
            &upload_url,
            &token,
            &args.id,
            args.interval,
            args.timeout,
            args.json,
        )?;
        return Ok(());
    }
    let submission = client::status(&upload_url, &token, &args.id)?;
    print_submission(&submission, args.json);
    if submission.state == client::ModerationState::Rejected {
        bail!(
            "submission {} was rejected: {}",
            submission.id,
            submission.reason.as_deref().unwrap_or("no reason given")
        )
    }
    Ok(())
}

/// Build every declared target, then upload each artifact. This is the
/// "one command" path: the author only names the source directory.
pub fn release(args: &ReleaseArgs) -> Result<()> {
    let report = rules::check(&args.source, &args.profiles, args.platform.as_deref())?;
    print_report(&report);
    if !report.ok() {
        bail!(
            "publication refused: {} rule violation(s); see docs/PUBLISHING-RULES.md",
            report.violations.len()
        )
    }
    let manifest = read_manifest(&args.source)?;
    let plans = target_plans(args, &manifest)?;
    let output_dir = args
        .output_dir
        .clone()
        .unwrap_or_else(|| args.source.join("dist"));
    fs::create_dir_all(&output_dir).with_context(|| format!("create {}", output_dir.display()))?;

    let resolved = if args.dry_run {
        None
    } else {
        Some(credentials::resolve(
            &args.registry,
            args.upload_url.as_deref(),
            args.token.as_deref(),
            args.key.as_deref(),
        )?)
    };

    let mut built = Vec::new();
    for plan in &plans {
        let output = args
            .output
            .clone()
            .unwrap_or_else(|| artifact_path(&output_dir, &manifest, &plan.target));
        let artifact = pack_artifact(&args.source, &plan.profile, &output)?;
        let bytes = fs::read(&artifact)
            .with_context(|| format!("read built artifact {}", artifact.display()))?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        println!(
            "Packed {} {} for {}: {} ({}, sha256 {}).",
            manifest.name,
            manifest.version,
            plan.target,
            artifact.display(),
            human_size(bytes.len() as u64),
            &sha256[..12]
        );
        built.push((plan.target.clone(), artifact, bytes, sha256));
    }

    if args.dry_run {
        println!(
            "dry run: {} artifact(s) ready; nothing uploaded.",
            built.len()
        );
        return Ok(());
    }

    let resolved = resolved.expect("credentials resolved outside dry-run");
    let mut queued = Vec::new();
    for (target, artifact, bytes, sha256) in &built {
        match upload(
            &resolved.upload_url,
            &resolved.token,
            &resolved.key,
            &manifest,
            target,
            artifact,
            bytes,
            sha256,
        )? {
            Some(submission) => {
                println!(
                    "Queued submission {} ({}).",
                    submission.id,
                    submission.state.as_str()
                );
                queued.push(submission.id);
            }
            None => println!(
                "Published {} {} for {}.",
                manifest.name, manifest.version, target
            ),
        }
    }

    if args.wait && !queued.is_empty() {
        let mut failure = None;
        for id in &queued {
            if let Err(error) = follow(&resolved.upload_url, &resolved.token, id, 5, 900, false) {
                eprintln!("{error:#}");
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
    }
    Ok(())
}

fn target_plans(args: &ReleaseArgs, manifest: &SourceManifest) -> Result<Vec<TargetPlan>> {
    let profiles = if let Some(platform) = args.platform.as_deref() {
        vec![PlatformProfile::load(&args.profiles, platform)?]
    } else {
        let mut concrete = manifest
            .targets
            .iter()
            .filter(|target| target.as_str() != "any")
            .collect::<Vec<_>>();
        concrete.sort();
        concrete.dedup();
        if concrete.is_empty() {
            bail!(
                "cannot determine platforms: yam.yaml only declares target any; pass --platform or set YAM_PLATFORM"
            )
        }
        concrete
            .into_iter()
            .map(|target| {
                PlatformProfile::load(&args.profiles, target)
                    .with_context(|| format!("target {target:?} has no usable platform profile"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    if args.output.is_some() && profiles.len() > 1 {
        bail!("--output selects one artifact; pass --platform to build a single target")
    }
    Ok(profiles
        .into_iter()
        .map(|profile| {
            let target = rules::artifact_target(&manifest.targets, &profile);
            TargetPlan { target, profile }
        })
        .collect())
}

fn read_manifest(source: &Path) -> Result<SourceManifest> {
    let path = source.join("yam.yaml");
    serde_yaml::from_slice(&fs::read(&path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn resolve_platform(
    source: &Path,
    profiles: &Path,
    platform: Option<&str>,
) -> Result<PlatformProfile> {
    if let Some(platform) = platform {
        return PlatformProfile::load(profiles, platform);
    }
    let manifest = read_manifest(source)?;
    let mut targets: Vec<&str> = manifest
        .targets
        .iter()
        .map(String::as_str)
        .filter(|target| *target != "any")
        .collect();
    targets.sort_unstable();
    targets.dedup();
    match targets.as_slice() {
        [platform] => PlatformProfile::load(profiles, platform),
        [] => bail!(
            "cannot determine platform: yam.yaml only declares target any; pass --platform or set YAM_PLATFORM"
        ),
        _ => bail!(
            "cannot determine platform: yam.yaml declares multiple targets ({}); pass --platform or set YAM_PLATFORM",
            targets.join(", ")
        ),
    }
}

fn pack_artifact(source: &Path, profile: &PlatformProfile, output: &Path) -> Result<PathBuf> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
    }
    Manager::pack(source, profile, output)?;
    rules::verify_artifact(output)?;
    Ok(output.to_path_buf())
}

fn artifact_path(output_dir: &Path, manifest: &SourceManifest, target: &str) -> PathBuf {
    let base = manifest.name.replace('/', "-");
    output_dir.join(format!("{base}-{}-{target}.yampkg", manifest.version))
}

fn upload(
    upload_url: &str,
    token: &str,
    key_path: &Path,
    manifest: &SourceManifest,
    target: &str,
    artifact_path: &Path,
    artifact: &[u8],
    sha256: &str,
) -> Result<Option<client::Submission>> {
    let key = sign::load_key(key_path)?;
    let signature = sign::sign_upload(&key, &manifest.name, &manifest.version, target, sha256);
    let outcome = client::upload(
        upload_url,
        token,
        &client::Publication {
            name: &manifest.name,
            version: &manifest.version,
            target,
            sha256: &sha256,
            public_key: &signature.public_key,
            signature: &signature.signature,
        },
        artifact,
    )
    .with_context(|| format!("upload {}", artifact_path.display()))?;
    match outcome {
        client::UploadOutcome::Queued(submission) => {
            println!(
                "Queued {} {} for {}: submission {} ({}).",
                manifest.name,
                manifest.version,
                target,
                submission.id,
                submission.state.as_str()
            );
            Ok(Some(submission))
        }
        client::UploadOutcome::Accepted => Ok(None),
    }
}

pub fn auth(args: &AuthArgs) -> Result<()> {
    match &args.command {
        AuthCommand::Login(args) => auth_login(args),
        AuthCommand::Status(args) => auth_status(args),
        AuthCommand::Logout(args) => auth_logout(args),
    }
}

fn auth_login(args: &AuthLoginArgs) -> Result<()> {
    let mut store = credentials::Store::load()?;
    let existing = store.entry(&args.registry).cloned();
    let upload_url = args
        .upload_url
        .clone()
        .or_else(|| existing.as_ref().map(|entry| entry.upload_url.clone()))
        .unwrap_or_else(|| client::DEFAULT_UPLOAD_URL.into());
    let token = match args
        .token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => token.to_string(),
        None => prompt_token()?,
    };
    let key = args
        .key
        .clone()
        .or_else(|| existing.as_ref().and_then(|entry| entry.key_file.clone()))
        .context(
            "author key is required: pass --key <file> or set YAM_AUTHOR_KEY_FILE (the PEM downloaded when claiming the namespace)",
        )?;
    if !key.is_file() {
        bail!("author key is not a file: {}", key.display())
    }
    client::api_base(&upload_url)?;
    let mut login = existing.as_ref().and_then(|entry| entry.login.clone());
    if !args.no_verify {
        match client::whoami(&upload_url, &token)? {
            client::WhoAmI::Known(account) => {
                login = account.login.clone().or(login);
                if account.namespaces.is_empty() {
                    eprintln!("Token verified by the registry.");
                } else {
                    eprintln!("Token verified; namespaces: {}.", account.namespaces.join(", "));
                }
            }
            client::WhoAmI::Unsupported => eprintln!(
                "note: this registry does not expose /api/v2/whoami; the token was saved without verification"
            ),
        }
    }
    let key_file = if args.store_key {
        credentials::copy_key(&key, &args.registry)?
    } else {
        key.canonicalize().unwrap_or(key)
    };
    store.insert(
        &args.registry,
        credentials::StoredRegistry {
            upload_url: upload_url.clone(),
            token,
            key_file: Some(key_file.clone()),
            login: login.clone(),
        },
    );
    if let Err(err) = store.save() {
        if args.store_key {
            fs::remove_file(&key_file)?;
        }
        return Err(err);
    }
    if let Some(old) = existing {
        credentials::remove_obsolete_key(&args.registry, &old, &store)?;
    }
    println!(
        "Logged in to {} as {} (registry {}).",
        host_of(&upload_url),
        login.as_deref().unwrap_or("an unverified author"),
        args.registry
    );
    println!("Author key: {}", key_file.display());
    Ok(())
}

fn auth_status(args: &AuthStatusArgs) -> Result<()> {
    let store = credentials::Store::load()?;
    let entry = store.entry(&args.registry);
    let upload_url = args
        .upload_url
        .clone()
        .or_else(|| entry.map(|entry| entry.upload_url.clone()))
        .unwrap_or_else(|| client::DEFAULT_UPLOAD_URL.into());
    let token = entry
        .map(|entry| entry.token.trim())
        .filter(|value| !value.is_empty());
    let mut login = entry.and_then(|entry| entry.login.clone());
    if args.verify {
        let token = token.context("no stored token to verify; run `yam auth login`")?;
        match client::whoami(&upload_url, token)? {
            client::WhoAmI::Known(account) => {
                login = account.login.clone().or(login);
                eprintln!("Token verified by the registry.");
            }
            client::WhoAmI::Unsupported => {
                eprintln!("note: this registry does not expose /api/v2/whoami")
            }
        }
    }
    let key_file = entry.and_then(|entry| entry.key_file.clone());
    let key_present = key_file.as_ref().is_some_and(|path| path.is_file());
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "registry": args.registry,
                "upload_url": upload_url,
                "token": token.map(credentials::fingerprint),
                "key_file": key_file,
                "key_present": key_present,
                "login": login,
            }))?
        );
    } else {
        println!("registry {}", args.registry);
        println!("upload   {upload_url}");
        println!(
            "token    {}",
            token
                .map(credentials::fingerprint)
                .unwrap_or_else(|| "not stored".into())
        );
        println!(
            "key      {}",
            key_file
                .as_ref()
                .map(|path| format!(
                    "{} ({})",
                    path.display(),
                    if key_present { "present" } else { "missing" }
                ))
                .unwrap_or_else(|| "not configured".into())
        );
        println!("login    {}", login.as_deref().unwrap_or("unknown"));
    }
    Ok(())
}

fn auth_logout(args: &AuthLogoutArgs) -> Result<()> {
    let mut store = credentials::Store::load()?;
    if args.all {
        let count = store.registries.len();
        let removed = std::mem::take(&mut store.registries);
        store.save()?;
        for (name, entry) in removed {
            credentials::remove_obsolete_key(&name, &entry, &store)?;
        }
        println!("Removed {count} stored registry credential(s).");
        return Ok(());
    }
    let entry = store
        .registries
        .remove(&args.registry)
        .context("no stored credentials for registry")?;
    store.save()?;
    credentials::remove_obsolete_key(&args.registry, &entry, &store)?;
    println!("Logged out of registry {}.", args.registry);
    Ok(())
}

fn prompt_token() -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("publish token is required: pass --token or set YAM_PUBLISH_TOKEN")
    }
    eprintln!("Publish token (input is visible; prefer YAM_PUBLISH_TOKEN in CI):");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let token = line.trim();
    if token.is_empty() {
        bail!("no token entered")
    }
    Ok(token.to_string())
}

fn host_of(upload_url: &str) -> String {
    reqwest::Url::parse(upload_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| upload_url.to_string())
}

fn follow(
    upload_url: &str,
    token: &str,
    id: &str,
    interval: u64,
    timeout: u64,
    json: bool,
) -> Result<()> {
    let started = Instant::now();
    let options = client::WatchOptions {
        interval: Duration::from_secs(interval),
        timeout: (timeout > 0).then(|| Duration::from_secs(timeout)),
    };
    let final_submission =
        client::watch_submission(upload_url, token, id, &options, &mut |submission| {
            if json {
                print_submission_json(submission);
            } else {
                println!(
                    "{} {} ({}s)",
                    submission.id,
                    submission.state.as_str(),
                    started.elapsed().as_secs()
                );
                if let Some(reason) = &submission.reason {
                    println!("reason: {reason}");
                }
            }
        })?;
    if final_submission.state == client::ModerationState::Rejected {
        bail!(
            "submission {} was rejected: {}",
            final_submission.id,
            final_submission
                .reason
                .as_deref()
                .unwrap_or("no reason given")
        )
    }
    if !json {
        println!(
            "Submission {} is {}.",
            final_submission.id,
            final_submission.state.as_str()
        );
    }
    Ok(())
}

fn print_report(report: &rules::Report) {
    for violation in &report.violations {
        eprintln!("error[{}]: {}", violation.code, violation.message);
    }
    for note in &report.notes {
        eprintln!("note: {note}");
    }
    if report.ok() {
        println!(
            "{} {} passes publication rules for {}.",
            report.name,
            report.version,
            report.targets.join(", ")
        );
    }
}

fn print_report_json(report: &rules::Report) {
    let violations = report
        .violations
        .iter()
        .map(|violation| serde_json::json!({"code": violation.code, "message": violation.message}))
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "ok": report.ok(),
            "name": report.name,
            "version": report.version,
            "targets": report.targets,
            "violations": violations,
            "notes": report.notes,
        }))
        .unwrap_or_else(|_| "{\"ok\":false}".into())
    );
}

fn print_submission(submission: &client::Submission, json: bool) {
    if json {
        print_submission_json(submission);
    } else {
        println!("{} {}", submission.id, submission.state.as_str());
        if let Some(reason) = &submission.reason {
            println!("reason: {reason}");
        }
    }
}

fn print_submission_json(submission: &client::Submission) {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "id": submission.id,
            "state": submission.state.as_str(),
            "reason": submission.reason,
            "updated_at": submission.updated_at,
        }))
        .unwrap_or_else(|_| "{}".into())
    );
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn profile(dir: &Path, id: &str) {
        fs::write(
            dir.join(format!("{id}.yaml")),
            format!(
                "id: {id}\narch: aarch64\ntriple: aarch64-unknown-linux-musl\npaths:\n  root: /tmp/yam-test\n  etc: /tmp/yam-etc\n  var: /tmp/yam-var\n  run: /tmp/yam-run\n"
            ),
        )
        .unwrap();
    }

    fn release_tree(targets: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src/share/test.pkg")).unwrap();
        fs::create_dir_all(dir.path().join("profiles")).unwrap();
        fs::write(dir.path().join("src/share/test.pkg/readme.txt"), "hello").unwrap();
        fs::write(
            dir.path().join("src/yam.yaml"),
            format!(
                "name: test/pkg\nversion: 1.2.3\nlicense: MIT\ndescription: Test package\ntargets: [{targets}]\nfiles:\n  share/test.pkg/: share/test.pkg/\n"
            ),
        )
        .unwrap();
        for target in targets.split(',').map(str::trim) {
            if target != "any" {
                profile(&dir.path().join("profiles"), target);
            }
        }
        dir
    }

    fn release_args(dir: &TempDir) -> ReleaseArgs {
        ReleaseArgs {
            source: dir.path().join("src"),
            platform: None,
            profiles: dir.path().join("profiles"),
            output_dir: Some(dir.path().join("dist")),
            output: None,
            dry_run: true,
            registry: "official".into(),
            upload_url: None,
            token: None,
            key: None,
            wait: false,
        }
    }

    #[test]
    fn release_plans_one_artifact_per_declared_target() {
        let dir = release_tree("testboard, otherboard");
        let args = release_args(&dir);
        let manifest = read_manifest(&args.source).unwrap();
        let plans = target_plans(&args, &manifest).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.target.as_str())
                .collect::<Vec<_>>(),
            vec!["otherboard", "testboard"]
        );
    }

    #[test]
    fn explicit_output_requires_a_single_target() {
        let dir = release_tree("testboard, otherboard");
        let mut args = release_args(&dir);
        args.output = Some(dir.path().join("one.yampkg"));
        let manifest = read_manifest(&args.source).unwrap();
        assert!(target_plans(&args, &manifest).is_err());
    }

    #[test]
    fn artifact_name_is_stable_and_filesystem_safe() {
        let dir = release_tree("testboard");
        let args = release_args(&dir);
        let manifest = read_manifest(&args.source).unwrap();
        assert_eq!(
            artifact_path(&dir.path().join("dist"), &manifest, "testboard"),
            dir.path()
                .join("dist")
                .join("test-pkg-1.2.3-testboard.yampkg")
        );
    }

    #[test]
    fn dry_run_builds_artifacts_without_credentials() {
        let dir = release_tree("testboard");
        let args = release_args(&dir);
        release(&args).unwrap();
        assert!(dir
            .path()
            .join("dist/test-pkg-1.2.3-testboard.yampkg")
            .is_file());
    }

    #[test]
    fn a_target_without_a_profile_is_reported() {
        let dir = release_tree("testboard");
        let args = ReleaseArgs {
            platform: Some("nosuchboard".into()),
            ..release_args(&dir)
        };
        assert!(release(&args).is_err());
    }
}
