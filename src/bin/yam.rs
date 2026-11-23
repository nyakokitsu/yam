use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;
use yam::v2::registry;
use yam::v2::update::{self, UpdateResult};
use yam::v2::{Manager, PlatformProfile};

#[derive(Parser)]
#[command(name = "yam", about = "Y Application Manager")]
struct Cli {
    /// Platform profile ID, such as yandexmicro.
    #[arg(long, env = "YAM_PLATFORM")]
    platform: String,
    /// Directory containing platform YAML profiles. Defaults to
    /// `<root>/platforms`, `<yam binary dir>/../platforms`, then `./platforms`.
    #[arg(long, env = "YAM_PROFILES")]
    profiles: Option<PathBuf>,
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

/// Picks the profiles directory: an explicit value always wins; otherwise the
/// first existing of `<root>/platforms` (custom `YAM_ROOT`), `<exe dir>/../platforms`
/// (installed layout `$YAM_ROOT/bin/yam`) and `./platforms` (source tree).
/// When none exists, `./platforms` is returned so the load error names it.
fn resolve_profiles_dir(
    explicit: Option<PathBuf>,
    root: Option<&Path>,
    exe_dir: Option<&Path>,
) -> PathBuf {
    if let Some(dir) = explicit {
        return dir;
    }
    let candidates = [
        root.map(|root| root.join("platforms")),
        exe_dir.map(|dir| dir.join("..").join("platforms")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| PathBuf::from("platforms"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let profiles = resolve_profiles_dir(cli.profiles, cli.root.as_deref(), exe_dir.as_deref());
    let profile = PlatformProfile::load(&profiles, &cli.platform)?;
    let root = cli.root.unwrap_or_else(|| profile.paths.root.clone());
    let mut manager = Manager::open(root, profile)?;
    match cli.command {
        Command::Install { package, yes } => {
            let manifest = if Path::new(&package).is_file() {
                manager.install(Path::new(&package), yes)?
            } else {
                let (name, version) = split_package_version(&package)?;
                let selected = registry::registry(&manager_root(&manager), &cli.registry)?;
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
                println!("Состояние пакетов в порядке.");
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
        Command::Update => match update::apply()? {
            UpdateResult::Current(version) => {
                println!("Уже установлена актуальная версия {version}.")
            }
            UpdateResult::Updated(version) => println!("Установлено обновление {version}."),
        },
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
            let selected = registry::registry(&manager_root(&manager), &cli.registry)?;
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
            let selected = registry::registry(&manager_root(&manager), &cli.registry)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&registry::package_index(&selected, &name)?)?
            );
        }
        Command::Plan { package } => {
            let (name, version) = split_package_version(&package)?;
            let selected = registry::registry(&manager_root(&manager), &cli.registry)?;
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
    use super::resolve_profiles_dir;
    use std::{fs, path::PathBuf};
    use tempfile::tempdir;

    #[test]
    fn explicit_value_overrides_every_default() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("platforms")).unwrap();
        let explicit = PathBuf::from("/somewhere/else");
        let got = resolve_profiles_dir(Some(explicit.clone()), Some(tmp.path()), Some(tmp.path()));
        assert_eq!(got, explicit);
    }

    #[test]
    fn custom_root_uses_its_platforms_dir() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("platforms")).unwrap();
        let got = resolve_profiles_dir(None, Some(tmp.path()), None);
        assert_eq!(got, tmp.path().join("platforms"));
    }

    #[test]
    fn root_wins_over_binary_location() {
        let root = tempdir().unwrap();
        let installed = tempdir().unwrap();
        fs::create_dir(root.path().join("platforms")).unwrap();
        fs::create_dir(installed.path().join("platforms")).unwrap();
        let bin = installed.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let got = resolve_profiles_dir(None, Some(root.path()), Some(&bin));
        assert_eq!(got, root.path().join("platforms"));
    }

    #[test]
    fn installed_binary_finds_sibling_platforms_without_root() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("platforms")).unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let got = resolve_profiles_dir(None, None, Some(&bin));
        assert_eq!(got, bin.join("..").join("platforms"));
        assert!(got.is_dir());
    }

    #[test]
    fn missing_dirs_fall_back_to_source_tree_default() {
        let tmp = tempdir().unwrap();
        let got = resolve_profiles_dir(None, Some(tmp.path()), Some(tmp.path()));
        assert_eq!(got, PathBuf::from("platforms"));
    }
}
