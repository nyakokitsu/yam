use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};
use yam::publish::{client, rules, sign};
use yam::v2::{Manager, PlatformProfile};

#[derive(Parser)]
#[command(
    name = "yam-publish",
    about = "Build and publish a YAM package from a PC"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// Check publication rules without building an artifact.
    Check(CheckArgs),
    /// Build a .yampkg with a host YAM CLI.
    Pack(PackArgs),
    /// Sign and upload an already-built .yampkg.
    Publish(PublishArgs),
    /// Build, then sign and upload one target artifact.
    Release(ReleaseArgs),
    /// Show the moderation state of a published artifact.
    Status(StatusArgs),
}

#[derive(Args, Clone)]
struct CheckArgs {
    /// Directory containing yam.yaml.
    #[arg(long, default_value = ".")]
    source: PathBuf,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    profiles: PathBuf,
    /// Platform ID from the profiles directory. Defaults to the concrete
    /// target in yam.yaml when it declares exactly one.
    #[arg(long, env = "YAM_PLATFORM")]
    platform: Option<String>,
    /// Print a machine-readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone)]
struct PackArgs {
    /// Directory containing yam.yaml.
    #[arg(long, default_value = ".")]
    source: PathBuf,
    /// Platform ID from the profiles directory. Defaults to YAM_PLATFORM,
    /// then to the sole concrete target in yam.yaml.
    #[arg(long, env = "YAM_PLATFORM")]
    platform: Option<String>,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    profiles: PathBuf,
    /// Destination .yampkg path.
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Args, Clone)]
struct PublishArgs {
    /// Built .yampkg archive.
    #[arg(long)]
    artifact: PathBuf,
    /// Directory containing the source yam.yaml used to create the artifact.
    #[arg(long, default_value = ".")]
    source: PathBuf,
    /// Target used for this artifact.
    #[arg(long)]
    target: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL", default_value = client::DEFAULT_UPLOAD_URL)]
    upload_url: String,
    /// Publish token created in the registry profile.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    token: String,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    key: PathBuf,
    /// Follow the moderation queue until the artifact is approved or rejected.
    #[arg(long)]
    wait: bool,
}

#[derive(Args, Clone)]
struct ReleaseArgs {
    #[command(flatten)]
    pack: PackArgs,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL", default_value = client::DEFAULT_UPLOAD_URL)]
    upload_url: String,
    /// Publish token created in the registry profile.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    token: String,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    key: PathBuf,
    /// Follow the moderation queue until the artifact is approved or rejected.
    #[arg(long)]
    wait: bool,
}

#[derive(Args, Clone)]
struct StatusArgs {
    /// Submission id returned by `publish`.
    id: String,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL", default_value = client::DEFAULT_UPLOAD_URL)]
    upload_url: String,
    /// Publish token created in the registry profile.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    token: String,
    /// Keep polling until the submission is approved or rejected.
    #[arg(long)]
    watch: bool,
    /// Seconds between status requests while watching.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Give up after this many seconds; 0 waits forever.
    #[arg(long, default_value_t = 900)]
    timeout: u64,
    /// Print one JSON object per state instead of human-readable lines.
    #[arg(long)]
    json: bool,
}

#[derive(Deserialize)]
struct SourceManifest {
    name: String,
    version: String,
    #[serde(default)]
    targets: Vec<String>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        CommandKind::Check(args) => {
            let report = enforce_rules(&args.source, &args.profiles, args.platform.as_deref())?;
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
        CommandKind::Pack(args) => {
            enforce_rules(&args.source, &args.profiles, args.platform.as_deref())?;
            pack(&args)?;
            Ok(())
        }
        CommandKind::Publish(args) => publish(&args),
        CommandKind::Release(args) => {
            enforce_rules(
                &args.pack.source,
                &args.pack.profiles,
                args.pack.platform.as_deref(),
            )?;
            let target = pack(&args.pack)?;
            publish(&PublishArgs {
                artifact: args.pack.output.clone(),
                source: args.pack.source,
                target,
                upload_url: args.upload_url,
                token: args.token,
                key: args.key,
                wait: args.wait,
            })
        }
        CommandKind::Status(args) => status(&args),
    }
}

/// Evaluate the local publication rules. `pack`, `release` and `check` share
/// this so an author sees the same report no matter which entry point they use.
fn enforce_rules(
    source: &std::path::Path,
    profiles: &std::path::Path,
    platform: Option<&str>,
) -> Result<rules::Report> {
    rules::check(source, profiles, platform)
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

fn pack(args: &PackArgs) -> Result<String> {
    let profile = resolve_platform(args)?;
    Manager::pack(&args.source, &profile, &args.output)?;
    Ok(profile.id)
}

fn resolve_platform(args: &PackArgs) -> Result<PlatformProfile> {
    if let Some(platform) = &args.platform {
        return PlatformProfile::load(&args.profiles, platform);
    }
    let manifest_path = args.source.join("yam.yaml");
    let manifest: SourceManifest = serde_yaml::from_slice(
        &fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )?;
    let mut targets: Vec<&str> = manifest
        .targets
        .iter()
        .map(String::as_str)
        .filter(|target| *target != "any")
        .collect();
    targets.sort_unstable();
    targets.dedup();
    match targets.as_slice() {
        [platform] => PlatformProfile::load(&args.profiles, platform),
        [] => bail!(
            "cannot determine platform: yam.yaml only declares target any; pass --platform or set YAM_PLATFORM"
        ),
        _ => bail!(
            "cannot determine platform: yam.yaml declares multiple targets ({}); pass --platform or set YAM_PLATFORM",
            targets.join(", ")
        ),
    }
}

fn publish(args: &PublishArgs) -> Result<()> {
    let manifest_path = args.source.join("yam.yaml");
    let manifest: SourceManifest = serde_yaml::from_slice(
        &fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )?;
    let artifact =
        fs::read(&args.artifact).with_context(|| format!("read {}", args.artifact.display()))?;
    if artifact.is_empty() {
        bail!("artifact is empty")
    }
    let sha256 = format!("{:x}", Sha256::digest(&artifact));
    let key = sign::load_key(&args.key)?;
    let signature = sign::sign_upload(
        &key,
        &manifest.name,
        &manifest.version,
        &args.target,
        &sha256,
    );
    let outcome = client::upload(
        &args.upload_url,
        &args.token,
        &client::Publication {
            name: &manifest.name,
            version: &manifest.version,
            target: &args.target,
            sha256: &sha256,
            public_key: &signature.public_key,
            signature: &signature.signature,
        },
        &artifact,
    )?;
    match outcome {
        client::UploadOutcome::Queued(submission) => {
            println!(
                "Queued {} {} for {}: submission {} ({}).",
                manifest.name,
                manifest.version,
                args.target,
                submission.id,
                submission.state.as_str()
            );
            if args.wait {
                follow(&args.upload_url, &args.token, &submission.id, 5, 900, false)?;
            } else {
                println!(
                    "Check it with `yam-publish status {} --watch`.",
                    submission.id
                );
            }
        }
        client::UploadOutcome::Accepted => {
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

fn status(args: &StatusArgs) -> Result<()> {
    if args.watch {
        follow(
            &args.upload_url,
            &args.token,
            &args.id,
            args.interval,
            args.timeout,
            args.json,
        )?;
        return Ok(());
    }
    let submission = client::status(&args.upload_url, &args.token, &args.id)?;
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

fn follow(
    upload_url: &str,
    token: &str,
    id: &str,
    interval: u64,
    timeout: u64,
    json: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pack_platform_defaults_to_the_only_concrete_source_target() {
        let workdir = TempDir::new().unwrap();
        let source = workdir.path().join("source");
        let profiles = workdir.path().join("profiles");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profiles).unwrap();
        fs::write(
            source.join("yam.yaml"),
            "name: test/package\nversion: 1.0.0\ntargets: [testplatform]\n",
        )
        .unwrap();
        fs::write(
            profiles.join("testplatform.yaml"),
            "id: testplatform\narch: aarch64\ntriple: aarch64-unknown-linux-musl\npaths:\n  root: /tmp/yam-test\n  etc: /tmp/yam-etc\n  var: /tmp/yam-var\n  run: /tmp/yam-run\n",
        )
        .unwrap();
        let args = PackArgs {
            source,
            platform: None,
            profiles,
            output: workdir.path().join("out.yampkg"),
        };
        assert_eq!(resolve_platform(&args).unwrap().id, "testplatform");
    }
}
