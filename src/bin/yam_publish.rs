use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand};
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};
use reqwest::{blocking::Client, header::HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Duration};
use yam::publish::rules;
use yam::v2::{Manager, PlatformProfile};

const DEFAULT_UPLOAD_URL: &str = "https://repo.yse.wtf/api/v2/upload";

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
    #[arg(long, env = "YAM_UPLOAD_URL", default_value = DEFAULT_UPLOAD_URL)]
    upload_url: String,
    /// Publish token created in the registry profile.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    token: String,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    key: PathBuf,
}

#[derive(Args, Clone)]
struct ReleaseArgs {
    #[command(flatten)]
    pack: PackArgs,
    /// Registry upload endpoint, not the client /v2 index URL.
    #[arg(long, env = "YAM_UPLOAD_URL", default_value = DEFAULT_UPLOAD_URL)]
    upload_url: String,
    /// Publish token created in the registry profile.
    #[arg(long, env = "YAM_PUBLISH_TOKEN")]
    token: String,
    /// PEM PKCS#8 Ed25519 private key downloaded when claiming the namespace.
    #[arg(long, env = "YAM_AUTHOR_KEY_FILE")]
    key: PathBuf,
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
        CommandKind::Check(args) => enforce_rules(
            &args.source,
            &args.profiles,
            args.platform.as_deref(),
            args.json,
        ),
        CommandKind::Pack(args) => {
            enforce_rules(
                &args.source,
                &args.profiles,
                args.platform.as_deref(),
                false,
            )?;
            pack(&args)?;
            Ok(())
        }
        CommandKind::Publish(args) => publish(&args),
        CommandKind::Release(args) => {
            enforce_rules(
                &args.pack.source,
                &args.pack.profiles,
                args.pack.platform.as_deref(),
                false,
            )?;
            let target = pack(&args.pack)?;
            publish(&PublishArgs {
                artifact: args.pack.output.clone(),
                source: args.pack.source,
                target,
                upload_url: args.upload_url,
                token: args.token,
                key: args.key,
            })
        }
    }
}

/// Evaluate the local publication rules and print every violation. `pack`,
/// `release` and `check` share this so an author sees the same report no
/// matter which entry point they use.
fn enforce_rules(
    source: &std::path::Path,
    profiles: &std::path::Path,
    platform: Option<&str>,
    json: bool,
) -> Result<()> {
    let report = rules::check(source, profiles, platform)?;
    if json {
        let violations = report
            .violations
            .iter()
            .map(|violation| {
                serde_json::json!({"code": violation.code, "message": violation.message})
            })
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
            }))?
        );
    } else {
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
    if !report.ok() {
        bail!(
            "publication refused: {} rule violation(s); see docs/PUBLISHING-RULES.md",
            report.violations.len()
        )
    }
    Ok(())
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
    let key = SigningKey::from_pkcs8_pem(
        &fs::read_to_string(&args.key).with_context(|| format!("read {}", args.key.display()))?,
    )
    .context("author key must be an Ed25519 PKCS#8 PEM private key")?;
    let public_key = format!(
        "ed25519:{}",
        STANDARD.encode(key.verifying_key().to_bytes())
    );
    let signature = STANDARD.encode(
        key.sign(
            upload_message(&manifest.name, &manifest.version, &args.target, &sha256).as_bytes(),
        )
        .to_bytes(),
    );
    let response = Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("yam-publish")
        .build()?
        .post(&args.upload_url)
        .header("authorization", format!("Bearer {}", args.token))
        .header("content-type", "application/octet-stream")
        .header("x-yam-name", header(&manifest.name)?)
        .header("x-yam-version", header(&manifest.version)?)
        .header("x-yam-target", header(&args.target)?)
        .header("x-yam-sha256", &sha256)
        .header("x-yam-author-key", public_key)
        .header("x-yam-author-signature", signature)
        .body(artifact)
        .send()?;
    let status = response.status();
    let body = response.text().unwrap_or_default();
    if !status.is_success() {
        bail!("registry rejected publication ({status}): {}", body.trim())
    }
    println!(
        "Published {} {} for {}.",
        manifest.name, manifest.version, args.target
    );
    Ok(())
}

fn header(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).context("metadata contains an invalid HTTP header value")
}

fn upload_message(name: &str, version: &str, target: &str, sha256: &str) -> String {
    format!("yamreg-upload-v1\n{name}\n{version}\n{target}\n{sha256}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn upload_signature_binds_all_artifact_identity_fields() {
        assert_eq!(
            upload_message("author/package", "1.2.3", "yandexmini_2", "abc"),
            "yamreg-upload-v1\nauthor/package\n1.2.3\nyandexmini_2\nabc\n"
        );
    }

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
