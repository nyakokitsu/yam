use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::blocking::{multipart, Client};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

const DEFAULT_REGISTRY_INDEX: &str =
    "https://raw.githubusercontent.com/nyakokitsu/yapkg/registry/index.json";

#[derive(Parser)]
#[command(name = "yam", about = "YAM package manager")]
struct Cli {
    #[arg(long, env = "YAM_URL", default_value = "http://127.0.0.1:8099")]
    url: String,
    #[arg(long, env = "YAM_REGISTRY_URL")]
    registry: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Install {
        package: PathBuf,
        /// Version in the registry. When provided, package is treated as its registry ID.
        version: Option<String>,
    },
    List,
    Ps,
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Run {
        id: String,
        #[arg(short, long)]
        detach: bool,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Stop {
        id: String,
    },
    Rm {
        id: String,
    },
    Logs {
        id: String,
    },
    Inspect {
        id: String,
    },
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    Device,
    Sensors,
    Clock {
        #[command(subcommand)]
        command: ClockCommand,
    },
    Led {
        /// LED zone; the logo is available on compatible Midi hardware.
        #[arg(long, value_enum, default_value_t = LedZone::Ring)]
        zone: LedZone,
        #[command(subcommand)]
        command: LedCommand,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    List,
    Pull { id: String, version: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum LedZone {
    Ring,
    Logo,
}
impl LedZone {
    fn path(self) -> &'static str {
        match self {
            Self::Ring => "/api/led",
            Self::Logo => "/api/led/logo",
        }
    }
}

#[derive(Subcommand)]
enum LedCommand {
    Set { colors: Vec<String> },
    Off,
}

#[derive(Subcommand)]
enum ClockCommand {
    Status,
    Set {
        time: String,
        #[arg(short, long, default_value_t = 170)]
        brightness: u8,
    },
    Off,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::new();
    match cli.command {
        Command::Install { package, version } => {
            if let Some(version) = version {
                let id = package
                    .to_str()
                    .context("registry package ID must be valid UTF-8")?;
                install_registry_package(
                    &client,
                    &cli.url,
                    cli.registry.as_deref().unwrap_or(DEFAULT_REGISTRY_INDEX),
                    id,
                    &version,
                )?;
            } else {
                let file = fs::File::open(&package)
                    .with_context(|| format!("open {}", package.display()))?;
                let part = multipart::Part::reader(file).file_name(
                    package
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
                let response = client
                    .post(endpoint(&cli.url, "/api/plugins"))
                    .multipart(multipart::Form::new().part("package", part))
                    .send()?;
                print_response(response)?;
            }
        }
        Command::List => {
            let packages: Value =
                json_response(client.get(endpoint(&cli.url, "/api/plugins")).send()?)?;
            println!("{}", serde_json::to_string_pretty(&packages)?);
        }
        Command::Ps => {
            let packages: Value =
                json_response(client.get(endpoint(&cli.url, "/api/plugins")).send()?)?;
            let empty = Vec::new();
            let running = packages
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .filter(|package| package["running"] == true)
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&running)?);
        }
        Command::Enable { id } => set_enabled(&client, &cli.url, &id, true)?,
        Command::Disable { id } => set_enabled(&client, &cli.url, &id, false)?,
        Command::Run { id, detach, args } => {
            print_response(
                client
                    .post(endpoint(&cli.url, &format!("/api/plugins/{id}/invoke")))
                    .json(&serde_json::json!({"args": args}))
                    .send()?,
            )?;
            if !detach {
                wait_for_package(&client, &cli.url, &id)?;
            }
        }
        Command::Stop { id } => print_response(
            client
                .post(endpoint(&cli.url, &format!("/api/plugins/{id}/stop")))
                .send()?,
        )?,
        Command::Rm { id } => print_response(
            client
                .delete(endpoint(&cli.url, &format!("/api/plugins/{id}")))
                .send()?,
        )?,
        Command::Logs { id } => print_response(
            client
                .get(endpoint(&cli.url, &format!("/api/plugins/{id}/logs")))
                .send()?,
        )?,
        Command::Inspect { id } => {
            let packages: Value =
                json_response(client.get(endpoint(&cli.url, "/api/plugins")).send()?)?;
            let package = packages
                .as_array()
                .and_then(|items| items.iter().find(|item| item["manifest"]["id"] == id))
                .context("package not found")?;
            println!("{}", serde_json::to_string_pretty(package)?);
        }
        Command::Registry { command } => {
            let registry = cli
                .registry
                .unwrap_or_else(|| DEFAULT_REGISTRY_INDEX.into());
            match command {
                RegistryCommand::List => {
                    let (_, packages) = registry_packages(&client, &registry)?;
                    println!("{}", serde_json::to_string_pretty(&packages)?);
                }
                RegistryCommand::Pull { id, version } => {
                    install_registry_package(&client, &cli.url, &registry, &id, &version)?;
                }
            }
        }
        Command::Device => print_response(client.get(endpoint(&cli.url, "/api/device")).send()?)?,
        Command::Sensors => print_response(client.get(endpoint(&cli.url, "/api/sensors")).send()?)?,
        Command::Clock { command } => match command {
            ClockCommand::Status => {
                print_response(client.get(endpoint(&cli.url, "/api/clock")).send()?)?
            }
            ClockCommand::Set { time, brightness } => print_response(
                client
                    .put(endpoint(&cli.url, "/api/clock"))
                    .json(&serde_json::json!({"time": time, "brightness": brightness}))
                    .send()?,
            )?,
            ClockCommand::Off => print_response(
                client
                    .put(endpoint(&cli.url, "/api/clock"))
                    .json(&serde_json::json!({"off": true}))
                    .send()?,
            )?,
        },
        Command::Led { zone, command } => match command {
            LedCommand::Set { colors } => print_response(
                client
                    .post(endpoint(&cli.url, zone.path()))
                    .json(&serde_json::json!({"colors":colors}))
                    .send()?,
            )?,
            LedCommand::Off => print_response(
                client
                    .post(endpoint(&cli.url, &format!("{}/off", zone.path())))
                    .send()?,
            )?,
        },
    }
    Ok(())
}

enum RegistrySource {
    Api,
    Static { index_url: String },
}

fn install_registry_package(
    client: &Client,
    yam_url: &str,
    registry: &str,
    id: &str,
    version: &str,
) -> Result<()> {
    let daemon_response = client
        .post(endpoint(yam_url, "/api/plugins/registry"))
        .json(&serde_json::json!({
            "id": id,
            "version": version,
            "registry": registry,
        }))
        .send();
    if let Ok(response) = daemon_response {
        if response.status().is_success() {
            return print_response(response);
        }
    }

    let (source, packages) = registry_packages(client, registry)?;
    let package = match source {
        RegistrySource::Api => client
            .get(endpoint(
                registry,
                &format!("/api/packages/{id}/{version}/download"),
            ))
            .send()?
            .error_for_status()?
            .bytes()?,
        RegistrySource::Static { index_url } => {
            let entry = packages
                .as_array()
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| entry["id"] == id && entry["version"] == version)
                })
                .context("package version not found in static registry")?;
            let package_url = entry["url"]
                .as_str()
                .context("static registry entry has no URL")?;
            let package = client
                .get(static_package_url(&index_url, package_url)?)
                .send()?
                .error_for_status()?
                .bytes()?;
            let expected = entry["sha256"]
                .as_str()
                .context("static registry entry has no SHA-256")?;
            let actual = format!("{:x}", Sha256::digest(&package));
            if actual != expected {
                bail!("package checksum mismatch")
            }
            package
        }
    };
    let part = multipart::Part::bytes(package.to_vec()).file_name(format!("{id}-{version}.yap"));
    print_response(
        client
            .post(endpoint(yam_url, "/api/plugins"))
            .multipart(multipart::Form::new().part("package", part))
            .send()?,
    )
}

fn registry_packages(client: &Client, registry: &str) -> Result<(RegistrySource, Value)> {
    let index_url = if registry.trim_end_matches('/').ends_with("index.json") {
        registry.to_string()
    } else {
        endpoint(registry, "/index.json")
    };
    let response = client.get(&index_url).send()?;
    if response.status().is_success() {
        let index: Value = response.json()?;
        let packages = index["packages"].clone();
        if !packages.is_array() {
            bail!("invalid static registry index")
        }
        return Ok((RegistrySource::Static { index_url }, packages));
    }
    if response.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(response.error_for_status().unwrap_err().into());
    }
    let packages = json_response(client.get(endpoint(registry, "/api/packages")).send()?)?;
    Ok((RegistrySource::Api, packages))
}

fn static_package_url(index_url: &str, package_url: &str) -> Result<String> {
    if reqwest::Url::parse(package_url).is_ok() {
        return Ok(package_url.into());
    }
    Ok(reqwest::Url::parse(index_url)?.join(package_url)?.into())
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn json_response(response: reqwest::blocking::Response) -> Result<Value> {
    let response = response.error_for_status()?;
    Ok(response.json()?)
}

fn print_response(response: reqwest::blocking::Response) -> Result<()> {
    let status = response.status();
    let text = response.text()?;
    if !status.is_success() {
        bail!("{}: {}", status, text)
    }
    if !text.trim().is_empty() {
        println!("{text}");
    }
    Ok(())
}

fn set_enabled(client: &Client, url: &str, id: &str, desired: bool) -> Result<()> {
    let packages = json_response(client.get(endpoint(url, "/api/plugins")).send()?)?;
    let enabled = packages
        .as_array()
        .and_then(|items| items.iter().find(|item| item["manifest"]["id"] == id))
        .context("package not found")?["enabled"]
        .as_bool()
        .unwrap_or(false);
    if enabled != desired {
        print_response(
            client
                .post(endpoint(url, &format!("/api/plugins/{id}/toggle")))
                .send()?,
        )?;
    }
    Ok(())
}

fn wait_for_package(client: &Client, url: &str, id: &str) -> Result<()> {
    loop {
        let packages = json_response(client.get(endpoint(url, "/api/plugins")).send()?)?;
        let running = packages
            .as_array()
            .and_then(|items| items.iter().find(|item| item["manifest"]["id"] == id))
            .context("package disappeared")?["running"]
            .as_bool()
            .unwrap_or(false);
        if !running {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
