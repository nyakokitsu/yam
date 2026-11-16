use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
use yam::publish::cli;
use yam::v2::registry;
use yam::v2::update::{self, UpdateResult};
use yam::v2::{Manager, PlatformProfile};

#[derive(Parser)]
#[command(name = "yam", about = "Y Application Manager")]
struct Cli {
    /// Platform profile ID, such as yandexmicro. Required for station
    /// commands; publishing and auth commands run on a PC without it.
    #[arg(long, env = "YAM_PLATFORM")]
    platform: Option<String>,
    /// Directory containing platform YAML profiles.
    #[arg(long, env = "YAM_PROFILES", default_value = "platforms")]
    profiles: PathBuf,
    /// Override the profile's persistent package root (useful for development).
    #[arg(long, env = "YAM_ROOT")]
    root: Option<PathBuf>,
    /// Configured registry used by package-name installation.
    #[arg(long, default_value = "official")]
    registry: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Install {
        package: String,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    Remove {
        name: String,
    },
    List,
    Files {
        name: String,
    },
    Which {
        path: String,
    },
    Doctor,
    Repair,
    Update,
    Entware {
        #[command(subcommand)]
        command: EntwareCommand,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    Search {
        query: String,
    },
    Info {
        name: String,
    },
    /// Resolve and display the registry dependency graph without changing disk state.
    Plan {
        package: String,
    },
    /// Check publication rules of a package source directory (PC).
    Check(cli::CheckArgs),
    /// Build a .yampkg for one platform (PC).
    Pack(cli::PackArgs),
    /// Sign and upload an already-built .yampkg (PC).
    Publish(cli::PublishArgs),
    /// Build every declared target and publish the artifacts (PC).
    Release(cli::ReleaseArgs),
    /// Show the moderation state of a published artifact (PC).
    Status(cli::StatusArgs),
    /// Store or inspect registry credentials (PC).
    Auth(cli::AuthArgs),
}
#[derive(Subcommand)]
enum EntwareCommand {
    Status,
    List,
}
#[derive(Subcommand)]
enum ServiceCommand {
    List,
}
#[derive(Subcommand)]
enum RegistryCommand {
    List,
    Add {
        name: String,
        url: String,
        #[arg(long)]
        key: String,
    },
    Remove {
        name: String,
    },
}

fn main() -> Result<()> {
    let Cli {
        platform,
        profiles,
        root,
        registry,
        command,
    } = Cli::parse();
    match command {
        Command::Check(args) => return cli::check(&args),
        Command::Pack(args) => return cli::pack(&args),
        Command::Publish(args) => return cli::publish(&args),
        Command::Release(args) => return cli::release(&args),
        Command::Status(args) => return cli::status(&args),
        Command::Auth(args) => return cli::auth(&args),
        command => station(platform, profiles, root, registry, command),
    }
}

/// Commands that need an installed station profile. Parsing stays global so
/// `yam --platform X install` keeps working, but the profile is loaded only
/// here: a desktop `yam` must not require a station to publish a package.
fn station(
    platform: Option<String>,
    profiles: PathBuf,
    root: Option<PathBuf>,
    registry_name: String,
    command: Command,
) -> Result<()> {
    let platform =
        platform.context("--platform (or YAM_PLATFORM) is required for station commands")?;
    let profile = PlatformProfile::load(&profiles, &platform)?;
    let root = root.unwrap_or_else(|| profile.paths.root.clone());
    let mut manager = Manager::open(root, profile)?;
    match command {
        Command::Install { package, yes } => {
            let manifest = if Path::new(&package).is_file() {
                manager.install(Path::new(&package), yes)?
            } else {
                let (name, version) = split_package_version(&package)?;
                let selected = registry::registry(&manager_root(&manager), &registry_name)?;
                install_registry_graph(&mut manager, &selected, name, version, yes)?
            };
            println!(
                "Installed {} {} for {}.",
                manifest.name, manifest.version, manifest.target
            );
        }
        Command::Remove { name } => {
            manager.remove(&name)?;
            println!("Removed {name}.");
        }
        Command::List => {
            for package in manager.list()? {
                println!("{}\t{}\t{}", package.name, package.version, package.target);
            }
        }
        Command::Files { name } => {
            for path in manager.files(&name)? {
                println!("{path}");
            }
        }
        Command::Which { path } => match manager.which(&path)? {
            Some(package) => println!("{package}"),
            None => bail!("{path} is not owned by a YAM package"),
        },
        Command::Doctor => {
            let problems = manager.doctor()?;
            if problems.is_empty() {
                println!("?????????????????? ?????????????? ?? ??????????????.");
            } else {
                for problem in problems {
                    eprintln!("problem: {problem}");
                }
                bail!("doctor found inconsistencies")
            }
        }
        Command::Repair => {
            let repaired = manager.repair()?;
            if repaired.is_empty() {
                println!("Nothing to repair.");
            } else {
                for path in repaired {
                    println!("Repaired {path}.");
                }
            }
        }
        Command::Update => {
            match update::apply()? {
                UpdateResult::Current(version) => {
                    println!("?????? ?????????????????????? ???????????????????? ???????????? {version}.")
                }
                UpdateResult::Updated(version) => {
                    println!("?????????????????????? ???????????????????? {version}.")
                }
            }
        }
        Command::Entware { command } => match command {
            EntwareCommand::Status => println!(
                "{}",
                if manager.entware_available() {
                    "available"
                } else {
                    "unavailable"
                }
            ),
            EntwareCommand::List => print!("{}", manager.entware_list()?),
        },
        Command::Service { command } => match command {
            ServiceCommand::List => {
                for service in manager.services()? {
                    println!(
                        "{}/{}\t{}\t{}",
                        service.package,
                        service.name,
                        if service.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        service.spec.exec.join(" ")
                    );
                }
            }
        },
        Command::Registry { command } => match command {
            RegistryCommand::List => {
                for (name, entry) in registry::load_config(&manager_root(&manager))?.registries {
                    println!("{name}\t{}", entry.url);
                }
            }
            RegistryCommand::Add { name, url, key } => {
                registry::add_registry(&manager_root(&manager), &name, &url, &key)?;
                println!("Added registry {name}.");
            }
            RegistryCommand::Remove { name } => {
                registry::remove_registry(&manager_root(&manager), &name)?;
                println!("Removed registry {name}.");
            }
        },
        Command::Search { query } => {
            let selected = registry::registry(&manager_root(&manager), &registry_name)?;
            for package in registry::catalogue(&selected)?
                .packages
                .into_iter()
                .filter(|entry| {
                    entry.name.contains(&query)
                        || entry.summary.to_lowercase().contains(&query.to_lowercase())
                        || entry
                            .keywords
                            .iter()
                            .any(|keyword| keyword.contains(&query))
                })
            {
                println!("{}\t{}\t{}", package.name, package.latest, package.summary);
            }
        }
        Command::Info { name } => {
            let selected = registry::registry(&manager_root(&manager), &registry_name)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&registry::package_index(&selected, &name)?)?
            );
        }
        Command::Plan { package } => {
            let (name, version) = split_package_version(&package)?;
            let selected = registry::registry(&manager_root(&manager), &registry_name)?;
            let installed = manager.installed_snapshot()?;
            let mut source = registry::LiveSource::new(&selected);
            let packages =
                registry::resolve_with(&mut source, name, version, manager.profile(), &installed)?;
            for package in packages {
                println!(
                    "{}\t{}\t{}",
                    package.name, package.version, package.artifact.url
                );
            }
        }
        Command::Check(_)
        | Command::Pack(_)
        | Command::Publish(_)
        | Command::Release(_)
        | Command::Status(_)
        | Command::Auth(_) => {
            unreachable!("publishing commands are dispatched before station setup")
        }
    }
    Ok(())
}
fn manager_root(manager: &Manager) -> PathBuf {
    manager.root().to_path_buf()
}
fn install_registry_graph(
    manager: &mut Manager,
    registry_config: &registry::Registry,
    name: &str,
    version: Option<&str>,
    yes: bool,
) -> Result<yam::v2::PackageManifest> {
    let installed_snapshot = manager.installed_snapshot()?;
    let mut source = registry::LiveSource::new(registry_config);
    let graph = registry::resolve_with(
        &mut source,
        name,
        version,
        manager.profile(),
        &installed_snapshot,
    )?;
    let installed = manager
        .list()?
        .into_iter()
        .map(|package| (package.name, package.version))
        .collect::<BTreeMap<_, _>>();
    let download_dir = manager
        .root()
        // `Manager::install` clears interrupted extraction directories under
        // `.staging` before validating an archive. Registry downloads must
        // therefore live in the normal cache tree until every graph member
        // has been installed.
        .join("cache")
        .join(format!("registry-{}", Uuid::new_v4()));
    fs::create_dir_all(&download_dir)?;
    let downloaded = (|| -> Result<Vec<(registry::ResolvedPackage, PathBuf)>> {
        graph
            .into_iter()
            .enumerate()
            .map(|(index, package)| {
                let path = download_dir.join(format!("{index}.yampkg"));
                registry::download_artifact(registry_config, &package.artifact, &path)?;
                Ok((package, path))
            })
            .collect()
    })();
    let downloaded = match downloaded {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_dir_all(&download_dir);
            return Err(error);
        }
    };
    let mut newly_installed = Vec::new();
    let mut root_manifest = None;
    for (package, artifact) in downloaded {
        if installed
            .get(&package.name)
            .is_some_and(|current| current == &package.version)
        {
            continue;
        }
        match manager.install(&artifact, yes) {
            Ok(manifest) => {
                if !installed.contains_key(&manifest.name) {
                    newly_installed.push(manifest.name.clone());
                }
                if manifest.name == name {
                    root_manifest = Some(manifest);
                }
            }
            Err(error) => {
                for installed in newly_installed.into_iter().rev() {
                    let _ = manager.remove(&installed);
                }
                let _ = fs::remove_dir_all(&download_dir);
                return Err(error.context("registry graph installation failed; newly installed dependencies were rolled back"));
            }
        }
    }
    let _ = fs::remove_dir_all(&download_dir);
    root_manifest.context("requested package was already installed or absent from registry plan")
}
fn split_package_version(value: &str) -> Result<(&str, Option<&str>)> {
    let (name, version) = value
        .rsplit_once('@')
        .map_or((value, None), |(name, version)| (name, Some(version)));
    if name.is_empty() || version.is_some_and(str::is_empty) {
        bail!("package name must be name or name@version")
    }
    Ok((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_commands_parse_without_a_platform() {
        let cli = Cli::try_parse_from(["yam", "auth", "status"]).unwrap();
        assert!(cli.platform.is_none());
        assert!(matches!(cli.command, Command::Auth(_)));
    }

    #[test]
    fn station_commands_still_accept_the_platform_flag() {
        let cli = Cli::try_parse_from(["yam", "--platform", "yandexmicro", "list"]).unwrap();
        assert_eq!(cli.platform.as_deref(), Some("yandexmicro"));
        assert!(matches!(cli.command, Command::List));
    }
}
