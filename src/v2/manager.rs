impl Manager {
    pub fn open(root: PathBuf, profile: PlatformProfile) -> Result<Self> {
        for directory in [
            root.join("pkgs"),
            root.join("bin"),
            root.join("share"),
            root.join("db"),
            root.join(".staging"),
        ] {
            fs::create_dir_all(directory)?;
        }
        let db = Connection::open(root.join("db/state.sqlite3"))?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS packages (
               name TEXT PRIMARY KEY, version TEXT NOT NULL, target TEXT NOT NULL,
               payload_path TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS package_files (
               package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
               path TEXT NOT NULL, sha256 TEXT NOT NULL,
               PRIMARY KEY(package_name, path)
             );
             CREATE TABLE IF NOT EXISTS links (
               path TEXT PRIMARY KEY, package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
               target TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS provides (
               capability TEXT NOT NULL,
               package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
               PRIMARY KEY(capability, package_name)
             );
              CREATE TABLE IF NOT EXISTS dependencies (
                package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
                requirement TEXT NOT NULL,
                PRIMARY KEY(package_name, requirement)
              );
              CREATE TABLE IF NOT EXISTS services (
                package_name TEXT NOT NULL REFERENCES packages(name) ON DELETE CASCADE,
                name TEXT NOT NULL,
                spec_yaml TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(package_name, name)
              );",
        )?;
        let interrupted = root.join("db/transaction.json").is_file();
        Ok(Self {
            root,
            profile,
            db,
            interrupted,
            #[cfg(test)]
            fail_after_links: None,
            #[cfg(test)]
            fail_after_link_removals: None,
            #[cfg(test)]
            archive_limits_override: None,
        })
    }

    fn transaction_marker_path(&self) -> PathBuf {
        self.root.join("db/transaction.json")
    }

    /// Remove every leftover directory under `.staging/`: scratch space
    /// for an in-progress install, safe to clear at the start of any
    /// mutating command.
    fn clear_stale_staging(&self) -> Result<()> {
        for entry in fs::read_dir(self.root.join(".staging"))? {
            fs::remove_dir_all(entry?.path())?;
        }
        Ok(())
    }

    /// Take an exclusive lock on `$YAM_RUN/yam.lock` for the whole of a
    /// mutating command (B3). Held by keeping the returned `File` alive;
    /// the lock releases automatically when it drops. Fails immediately
    /// (no blocking wait) if another yam transaction already holds it --
    /// a host scanning shared directories is expected to take a shared
    /// lock on the same path instead.
    fn lock_transaction(&self) -> Result<fs::File> {
        let run_dir = &self.profile.paths.run;
        fs::create_dir_all(run_dir).with_context(|| format!("create {}", run_dir.display()))?;
        let lock_path = run_dir.join("yam.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "another yam transaction is running (locked: {})",
                lock_path.display()
            )
        })?;
        Ok(file)
    }

    pub fn profile(&self) -> &PlatformProfile {
        &self.profile
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Test-only: make the Nth link `install_payload` creates fail, to
    /// exercise the rollback path. See the `fail_after_links` field.
    #[cfg(test)]
    pub(crate) fn inject_link_failure_after(&mut self, count: u32) {
        self.fail_after_links = Some(count);
    }

    /// Test-only: make the Nth link `remove` removes fail, to exercise
    /// its rollback path. See the `fail_after_link_removals` field.
    #[cfg(test)]
    pub(crate) fn inject_remove_failure_after(&mut self, count: u32) {
        self.fail_after_link_removals = Some(count);
    }

    /// Test-only: override the archive extraction limits (B4).
    #[cfg(test)]
    pub(crate) fn inject_archive_limits(&mut self, limits: ArchiveLimits) {
        self.archive_limits_override = Some(limits);
    }

    fn archive_limits(&self) -> ArchiveLimits {
        #[cfg(test)]
        if let Some(limits) = &self.archive_limits_override {
            return limits.clone();
        }
        ArchiveLimits::DEFAULT
    }

    pub fn pack(source_dir: &Path, profile: &PlatformProfile, output: &Path) -> Result<()> {
        let descriptor = source_dir.join("yam.yaml");
        let source: PackageSource = serde_yaml::from_slice(
            &fs::read(&descriptor).with_context(|| format!("read {}", descriptor.display()))?,
        )?;
        validate_name(&source.name)?;
        validate_version(&source.version)?;
        if source.description.trim().is_empty() || source.license.trim().is_empty() {
            bail!("name, license and non-empty description are required")
        }
        if !source
            .targets
            .iter()
            .any(|target| profile.accepts_target(target))
        {
            bail!("{} does not support target {}", source.name, profile.id)
        }
        let target = if source.targets.iter().any(|target| target == &profile.id)
            || source
                .targets
                .iter()
                .any(|target| profile.aliases.iter().any(|alias| alias == target))
        {
            profile.id.clone()
        } else {
            "any".into()
        };
        let mut payload = Vec::<(String, PathBuf)>::new();
        let mut hashes = BTreeMap::new();
        for (destination, template) in &source.files {
            validate_payload_path(destination)?;
            let source_path = source_dir.join(expand(template, profile));
            if source_path.is_dir() {
                if !destination.ends_with('/') {
                    bail!("directory source {template:?} requires a destination ending in /")
                }
                for entry in walkdir::WalkDir::new(&source_path).sort_by_file_name() {
                    let entry = entry?;
                    if entry.file_type().is_symlink() {
                        bail!("symlink in source payload: {}", entry.path().display())
                    }
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let relative = entry.path().strip_prefix(&source_path)?;
                    let item = format!(
                        "{}{}",
                        destination.trim_end_matches('/'),
                        if destination.ends_with('/') {
                            format!("/{}", relative.display())
                        } else {
                            String::new()
                        }
                    );
                    validate_payload_path(&item)?;
                    insert_payload(&mut payload, &mut hashes, item, entry.path().to_path_buf())?;
                }
            } else {
                if !source_path.is_file() {
                    bail!("payload source is not a file: {}", source_path.display())
                }
                insert_payload(&mut payload, &mut hashes, destination.clone(), source_path)?;
            }
        }
        if payload.is_empty() {
            bail!("package has no payload files")
        }
        let manifest = PackageManifest {
            name: source.name,
            version: source.version,
            license: source.license,
            description: source.description,
            homepage: source.homepage,
            target,
            arch: profile.arch.clone(),
            depends: source.depends,
            recommends: source.recommends,
            enhances: source.enhances,
            provides: source.provides,
            conflicts: source.conflicts,
            replaces: source.replaces,
            permissions: source.permissions,
            entware: source.entware,
            services: source.services,
            postinstall: source.postinstall,
            files_sha256: hashes,
        };
        validate_manifest(&manifest)?;
        let file =
            fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
        let encoder = zstd::Encoder::new(file, 19)?;
        let mut archive = Builder::new(encoder.auto_finish());
        append_bytes(
            &mut archive,
            "MANIFEST.yaml",
            &serde_yaml::to_string(&manifest)?.into_bytes(),
        )?;
        for (destination, source) in payload {
            archive.append_path_with_name(source, format!("files/{destination}"))?;
        }
        archive.finish()?;
        Ok(())
    }

    pub fn install(&mut self, artifact: &Path, yes: bool) -> Result<PackageManifest> {
        let _lock = self.lock_transaction()?;
        if self.interrupted {
            bail!("a previous yam transaction did not finish; run `yam repair` before installing")
        }
        self.clear_stale_staging()?;
        let staging = self.root.join(".staging").join(Uuid::new_v4().to_string());
        fs::create_dir_all(staging.join("files"))?;
        let mut manifest = match extract_artifact(artifact, &staging, &self.archive_limits()) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        validate_manifest(&manifest)?;
        if !self.profile.accepts_target(&manifest.target) {
            bail!(
                "package target {} does not match platform {}",
                manifest.target,
                self.profile.id
            )
        }
        if manifest.target != "any" && manifest.target != self.profile.id {
            // Keep the database and `yam list` on the device profile id even
            // when the artifact came from an alias target.
            manifest.target = self.profile.id.clone();
        }
        if manifest.arch != self.profile.arch {
            bail!(
                "package architecture {} does not match {}",
                manifest.arch,
                self.profile.arch
            )
        }
        let to_replace = self.plan_relations(&manifest)?;
        if !to_replace.is_empty() && !yes {
            bail!(
                "{} would replace {}; rerun with --yes after reviewing the removal",
                manifest.name,
                to_replace.join(", ")
            )
        }
        if manifest
            .entware
            .as_ref()
            .is_some_and(|request| !request.packages.is_empty())
            && !yes
        {
            bail!(
                "{} requests Entware packages; rerun with --yes after reviewing them",
                manifest.name
            )
        }
        self.ensure_entware(&manifest, yes)?;
        let result = self.install_payload(&manifest, &staging);
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
        // A4: now that the replacement is installed and owns whatever
        // common-tree paths it claims, remove what it replaces. Skips
        // remove_locked's own dependents check: check_replace_dependents
        // (run as part of plan_relations, before any of this) already
        // accounts for this manifest covering what it replaces.
        for package in &to_replace {
            self.remove_locked(package, false)
                .with_context(|| format!("remove replaced package {package}"))?;
        }
        Ok(manifest)
    }

    fn install_payload(&mut self, manifest: &PackageManifest, staging: &Path) -> Result<()> {
        let (namespace, package) = split_name(&manifest.name)?;
        let destination = self
            .root
            .join("pkgs")
            .join(namespace)
            .join(package)
            .join(&manifest.version);

        // Refuse before touching disk if a linkable path collides with
        // something YAM does not own: a different package's link, or a
        // plain file/symlink nobody created through YAM.
        for path in manifest.files_sha256.keys().filter(|path| linkable(path)) {
            let owner: Option<String> = self
                .db
                .query_row(
                    "SELECT package_name FROM links WHERE path=?1",
                    [path],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                Some(owner) if owner == manifest.name => {}
                Some(owner) if manifest.replaces.iter().any(|name| name == &owner) => {}
                Some(owner) => bail!("{path} is already owned by {owner}"),
                None => {
                    let link = self.root.join(path);
                    if link.is_symlink() || link.exists() {
                        bail!(
                            "refusing to overwrite {path}: {} is not a YAM-owned link",
                            describe_obstruction(&link)
                        )
                    }
                }
            }
        }

        // Payload files are already written under `staging/files/` by
        // `extract_artifact`; only the directory itself is renamed into
        // place below.
        let files_dir = staging.join("files");
        let marker = self.transaction_marker_path();
        // Keep the hook context independent of the SQLite transaction's
        // mutable borrow of `self` below.
        let hook_root = self.root.clone();
        let hook_profile = self.profile.clone();
        let hook_has_entware = self.entware_available();
        let transaction = self.db.transaction()?;
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT version, payload_path FROM packages WHERE name=?1",
                [&manifest.name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_version, _)) = &existing {
            if old_version == &manifest.version {
                bail!(
                    "{} {} is already installed",
                    manifest.name,
                    manifest.version
                )
            }
        }
        // Links the previous version (if any) owns, captured before the
        // upcoming DELETE so an upgrade can tell which ones the new
        // version no longer wants and remove them from disk (A1).
        let old_links: Vec<(String, String)> = {
            let mut statement =
                transaction.prepare("SELECT path, target FROM links WHERE package_name=?1")?;
            let rows = statement
                .query_map([&manifest.name], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        if destination.exists() {
            bail!(
                "payload destination already exists: {}",
                destination.display()
            )
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let package_root = destination.parent().context("package root")?.to_path_buf();

        // Every metadata write below lives in `transaction`, which is
        // never committed unless the disk phase that follows succeeds
        // completely; dropping it without committing rolls all of this
        // back automatically (B1).
        transaction.execute(
            "DELETE FROM package_files WHERE package_name=?1",
            [&manifest.name],
        )?;
        transaction.execute(
            "DELETE FROM provides WHERE package_name=?1",
            [&manifest.name],
        )?;
        transaction.execute(
            "DELETE FROM dependencies WHERE package_name=?1",
            [&manifest.name],
        )?;
        transaction.execute(
            "DELETE FROM services WHERE package_name=?1",
            [&manifest.name],
        )?;
        transaction.execute(
            "INSERT INTO packages(name,version,target,payload_path) VALUES(?1,?2,?3,?4)
             ON CONFLICT(name) DO UPDATE SET version=excluded.version,target=excluded.target,payload_path=excluded.payload_path",
            params![manifest.name, manifest.version, manifest.target, destination.to_string_lossy()],
        )?;
        for capability in &manifest.provides {
            validate_capability(capability)?;
            transaction.execute(
                "INSERT INTO provides(capability,package_name) VALUES(?1,?2)",
                params![capability, manifest.name],
            )?;
        }
        for requirement in &manifest.depends {
            transaction.execute(
                "INSERT INTO dependencies(package_name,requirement) VALUES(?1,?2)",
                params![manifest.name, requirement],
            )?;
        }
        for (name, spec) in &manifest.services {
            transaction.execute(
                "INSERT INTO services(package_name,name,spec_yaml) VALUES(?1,?2,?3)",
                params![manifest.name, name, serde_yaml::to_string(spec)?],
            )?;
        }
        transaction.execute("DELETE FROM links WHERE package_name=?1", [&manifest.name])?;
        for (path, hash) in &manifest.files_sha256 {
            transaction.execute(
                "INSERT INTO package_files(package_name,path,sha256) VALUES(?1,?2,?3)",
                params![manifest.name, path, hash],
            )?;
            if linkable(path) {
                let target = destination.join(path);
                transaction.execute(
                    "INSERT OR REPLACE INTO links(path,package_name,target) VALUES(?1,?2,?3)",
                    params![path, manifest.name, target.to_string_lossy()],
                )?;
            }
        }

        // Disk phase: every step is recorded so a failure partway
        // through can be undone in reverse, leaving the disk exactly as
        // it was before this call (B1).
        let new_paths: HashSet<&str> = manifest
            .files_sha256
            .keys()
            .map(String::as_str)
            .filter(|path| linkable(path))
            .collect();
        let mut undo: Vec<Undo> = Vec::new();
        let mut removed_stale_links: Vec<PathBuf> = Vec::new();
        let mut links_created: u32 = 0;
        // Recorded before the first disk write so a crash partway
        // through is visible to the next `Manager::open` (B1); cleared
        // once this call either finishes or fully rolls back.
        fs::write(
            &marker,
            format!(
                r#"{{"op":"install","package":"{}","version":"{}"}}"#,
                manifest.name, manifest.version
            ),
        )?;
        let disk_result: Result<()> = (|| {
            fs::rename(&files_dir, &destination)?;
            undo.push(Undo::RemoveDirAll(destination.clone()));

            let current_link = package_root.join("current");
            let previous_current = fs::read_link(&current_link).ok();
            replace_link(&current_link, Path::new(&manifest.version))?;
            undo.push(match previous_current {
                Some(previous) => Undo::RestoreSymlink {
                    path: current_link.clone(),
                    target: previous,
                },
                None => Undo::RemoveSymlinkFile(current_link.clone()),
            });

            for path in manifest.files_sha256.keys().filter(|path| linkable(path)) {
                let link = self.root.join(path);
                if let Some(parent) = link.parent() {
                    fs::create_dir_all(parent)?;
                }
                let target = destination.join(path);
                let previous_target = fs::read_link(&link).ok();
                if link.exists() || link.is_symlink() {
                    fs::remove_file(&link)?;
                }
                make_link(&link, &target)?;
                undo.push(match previous_target {
                    Some(previous) => Undo::RestoreSymlink {
                        path: link.clone(),
                        target: previous,
                    },
                    None => Undo::RemoveSymlinkFile(link.clone()),
                });
                links_created += 1;
                #[cfg(test)]
                if self.fail_after_links == Some(links_created) {
                    bail!("injected test failure after creating {links_created} links")
                }
            }

            // A1: remove links the previous version owned that the new
            // version doesn't re-create, but only if the on-disk symlink
            // still points exactly where the database says it does --
            // never touch a path something else has since taken over.
            for (path, target) in &old_links {
                if new_paths.contains(path.as_str()) {
                    continue;
                }
                let link = self.root.join(path);
                if fs::read_link(&link).ok().as_deref() == Some(Path::new(target)) {
                    fs::remove_file(&link)
                        .with_context(|| format!("remove stale link {}", link.display()))?;
                    undo.push(Undo::RestoreSymlink {
                        path: link.clone(),
                        target: PathBuf::from(target),
                    });
                    removed_stale_links.push(link);
                }
            }
            Ok(())
        })();

        if let Err(error) = disk_result {
            let rollback_errors = unwind(undo);
            // Dropping `transaction` without committing rolls back every
            // database write above; drop it explicitly here so the
            // pruning call below can borrow `self` again.
            drop(transaction);
            // `create_dir_all(destination.parent())` above may have made
            // pkgs/<namespace>/<name> (and pkgs/<namespace>) from nothing;
            // Undo::RemoveDirAll only removed `destination` itself, so
            // clean those up too, stopping at pkgs/ itself.
            self.prune_empty_ancestors(&destination, &self.root.join("pkgs"));
            if rollback_errors.is_empty() {
                // The rollback fully succeeded: disk and database agree
                // again, so there is nothing left for `yam repair` to do.
                let _ = fs::remove_file(&marker);
                return Err(error);
            }
            // Otherwise leave the marker in place: the disk may still
            // disagree with the database, and only `yam repair` should
            // touch it next, not another install/remove.
            return Err(error.context(format!(
                "disk rollback left problems behind, run `yam repair`: {}",
                rollback_errors.join("; ")
            )));
        }

        if let Err(error) = Self::run_postinstall(
            manifest,
            &destination,
            &hook_root,
            &hook_profile,
            hook_has_entware,
        ) {
            let rollback_errors = unwind(undo);
            drop(transaction);
            self.prune_empty_ancestors(&destination, &self.root.join("pkgs"));
            if rollback_errors.is_empty() {
                let _ = fs::remove_file(&marker);
                return Err(error.context(format!("postinstall failed for {}", manifest.name)));
            }
            return Err(error.context(format!(
                "postinstall failed for {}; disk rollback left problems behind, run `yam repair`: {}",
                manifest.name,
                rollback_errors.join("; ")
            )));
        }

        transaction.commit()?;
        for link in &removed_stale_links {
            let floor = self.common_tree_floor(link);
            self.prune_empty_ancestors(link, &floor);
        }
        // A1: now that the database agrees the old version is gone,
        // remove its payload directory too.
        if let Some((old_version, _)) = &existing {
            if old_version != &manifest.version {
                let old_payload = package_root.join(old_version);
                fs::remove_dir_all(&old_payload).with_context(|| {
                    format!("remove old version directory {}", old_payload.display())
                })?;
            }
        }
        fs::remove_file(&marker).with_context(|| "remove the transaction marker".to_string())?;
        let _ = fs::remove_dir_all(staging);
        Ok(())
    }

    /// Run a verified package hook without a shell. The hook is executed
    /// while the database transaction and package lock are still held, so a
    /// non-zero exit or timeout rolls its package filesystem changes back.
    fn run_postinstall(
        manifest: &PackageManifest,
        package_dir: &Path,
        root: &Path,
        profile: &PlatformProfile,
        has_entware: bool,
    ) -> Result<()> {
        let Some((executable, arguments)) = manifest.postinstall.split_first() else {
            return Ok(());
        };
        let executable = package_dir.join(executable);
        let path = if has_entware {
            "/opt/bin:/opt/sbin:/usr/sbin:/usr/bin:/sbin:/bin"
        } else {
            "/usr/sbin:/usr/bin:/sbin:/bin"
        };
        let mut child = Command::new(&executable)
            .args(arguments)
            .current_dir(package_dir)
            .env_clear()
            .env("PATH", path)
            .env("YAM_ROOT", root)
            .env("YAM_PKG", package_dir)
            .env("YAM_ETC", &profile.paths.etc)
            .env("YAM_VAR", &profile.paths.var)
            .env("YAM_RUN", &profile.paths.run)
            .env("YAM_PLATFORM", &profile.id)
            .spawn()
            .with_context(|| format!("start postinstall {}", executable.display()))?;
        let timeout = std::time::Duration::from_secs(60);
        let status = wait_timeout::ChildExt::wait_timeout(&mut child, timeout)?;
        let status = match status {
            Some(status) => status,
            None => {
                child.kill()?;
                let _ = child.wait();
                bail!("postinstall timed out after {} seconds", timeout.as_secs())
            }
        };
        if !status.success() {
            bail!("postinstall exited with {status}")
        }
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<InstalledPackage>> {
        let mut statement = self
            .db
            .prepare("SELECT name,version,target FROM packages ORDER BY name")?;
        let packages = statement
            .query_map([], |row| {
                Ok(InstalledPackage {
                    name: row.get(0)?,
                    version: row.get(1)?,
                    target: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(packages)
    }

    /// A snapshot of what's installed, for the registry resolver (B2)
    /// to tell an already-satisfied dependency or short name from one
    /// it actually needs to resolve.
    pub fn installed_snapshot(&self) -> Result<registry::InstalledSnapshot> {
        let mut packages = BTreeMap::new();
        let mut statement = self.db.prepare("SELECT name, version FROM packages")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (name, version) = row?;
            packages.insert(name, version::parse_version(&version)?);
        }
        let mut provides: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut statement = self
            .db
            .prepare("SELECT capability, package_name FROM provides")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (capability, package_name) = row?;
            provides.entry(capability).or_default().insert(package_name);
        }
        Ok(registry::InstalledSnapshot { packages, provides })
    }

    pub fn files(&self, name: &str) -> Result<Vec<String>> {
        validate_name(name)?;
        let mut statement = self
            .db
            .prepare("SELECT path FROM package_files WHERE package_name=?1 ORDER BY path")?;
        let files = statement
            .query_map([name], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(files)
    }

    pub fn services(&self) -> Result<Vec<InstalledService>> {
        let mut statement = self.db.prepare(
            "SELECT package_name,name,enabled,spec_yaml FROM services ORDER BY package_name,name",
        )?;
        let services: Vec<InstalledService> = statement
            .query_map([], |row| {
                let raw: String = row.get(3)?;
                let spec = serde_yaml::from_str(&raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        raw.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(InstalledService {
                    package: row.get(0)?,
                    name: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    spec,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(services)
    }

    pub fn which(&self, path: &str) -> Result<Option<String>> {
        validate_payload_path(path)?;
        Ok(self
            .db
            .query_row(
                "SELECT package_name FROM links WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        let _lock = self.lock_transaction()?;
        if self.interrupted {
            bail!("a previous yam transaction did not finish; run `yam repair` before removing")
        }
        self.clear_stale_staging()?;
        self.remove_locked(name, true)
    }

    /// Core of `remove`, callable while a transaction lock is already
    /// held. `check_dependents` is false only when `install` removes a
    /// package `replaces` covers: `plan_relations`/`check_replace_dependents`
    /// already verified every dependent stays satisfied (directly or
    /// through the manifest being installed), which this blunter,
    /// capability-blind check cannot know about.
    fn remove_locked(&mut self, name: &str, check_dependents: bool) -> Result<()> {
        validate_name(name)?;
        let (namespace, package) = split_name(name)?;
        if check_dependents {
            let capabilities = {
                let mut statement = self
                    .db
                    .prepare("SELECT capability FROM provides WHERE package_name=?1")?;
                let values = statement
                    .query_map([name], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<HashSet<_>>>()?;
                values
            };
            let dependents = {
                let mut statement = self.db.prepare(
                    "SELECT package_name,requirement FROM dependencies WHERE package_name != ?1",
                )?;
                let dependents = statement
                    .query_map([name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .filter_map(Result::ok)
                    .filter(|(_, requirement)| {
                        requirement_name(requirement).is_ok_and(|required| {
                            required == name || capabilities.contains(required)
                        })
                    })
                    .map(|(package, _)| package)
                    .collect::<Vec<_>>();
                dependents
            };
            if !dependents.is_empty() {
                bail!(
                    "cannot remove {name}; required by {}",
                    dependents.join(", ")
                )
            }
        }
        self.db
            .query_row::<String, _, _>(
                "SELECT payload_path FROM packages WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?
            .context("package is not installed")?;
        let links = {
            let mut statement = self
                .db
                .prepare("SELECT path,target FROM links WHERE package_name=?1")?;
            let links = statement
                .query_map([name], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            links
        };
        let marker = self.transaction_marker_path();
        let transaction = self.db.transaction()?;
        transaction.execute("DELETE FROM packages WHERE name=?1", [name])?;

        // Disk phase: record each removed link so a failure partway
        // through can be undone, leaving every link exactly as it was
        // (B1), with the uncommitted transaction already guaranteeing
        // the database side reverts to "still installed".
        let mut undo: Vec<Undo> = Vec::new();
        let mut links_removed: u32 = 0;
        // See install_payload: recorded before the first disk write so a
        // crash partway through is visible on the next open (B1).
        fs::write(&marker, format!(r#"{{"op":"remove","package":"{name}"}}"#))?;
        let disk_result: Result<()> = (|| {
            for (path, target) in &links {
                let link = self.root.join(path);
                if link.is_symlink()
                    && fs::read_link(&link).ok().as_deref() == Some(Path::new(target))
                {
                    fs::remove_file(&link)
                        .with_context(|| format!("remove link {}", link.display()))?;
                    undo.push(Undo::RestoreSymlink {
                        path: link,
                        target: PathBuf::from(target),
                    });
                    links_removed += 1;
                    #[cfg(test)]
                    if self.fail_after_link_removals == Some(links_removed) {
                        bail!("injected test failure after removing {links_removed} links")
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = disk_result {
            let rollback_errors = unwind(undo);
            // Drop `transaction` explicitly (without committing, which
            // rolls the packages-row deletion back) so the marker
            // cleanup below can borrow `self` again.
            drop(transaction);
            if rollback_errors.is_empty() {
                let _ = fs::remove_file(&marker);
                return Err(error);
            }
            return Err(error.context(format!(
                "disk rollback left problems behind, run `yam repair`: {}",
                rollback_errors.join("; ")
            )));
        }
        transaction.commit()?;
        // A1: prune common-tree directories left empty by the links just
        // removed, then delete every version directory (not just the
        // current one) plus `current` itself, and the namespace
        // directory too if it is now empty.
        for (path, _) in &links {
            let link = self.root.join(path);
            let floor = self.common_tree_floor(&link);
            self.prune_empty_ancestors(&link, &floor);
        }
        let package_root = self.root.join("pkgs").join(namespace).join(package);
        fs::remove_dir_all(&package_root)
            .with_context(|| format!("remove package directory {}", package_root.display()))?;
        let namespace_root = self.root.join("pkgs").join(namespace);
        if fs::read_dir(&namespace_root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&namespace_root);
        }
        fs::remove_file(&marker).with_context(|| "remove the transaction marker".to_string())?;
        Ok(())
    }

    /// Remove `start`'s parent directory, then its parent, and so on,
    /// stopping as soon as a directory is non-empty or equals `floor`.
    /// Used after removing a link, or after rolling back a directory
    /// `create_dir_all` made room for, so an now-empty ancestor (e.g. a
    /// package's own `share/host/plugins/vasya.plug/`, or
    /// `pkgs/vasya/plug` once its only version directory is gone) doesn't
    /// linger forever. Never removes a directory with anything left in
    /// it, owned by YAM or not, and never removes `floor` itself.
    fn prune_empty_ancestors(&self, start: &Path, floor: &Path) {
        let mut current = start.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            if dir == floor || !dir.starts_with(&self.root) {
                break;
            }
            match fs::read_dir(&dir) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
            if fs::remove_dir(&dir).is_err() {
                break;
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }

    /// The `bin/` or `share/` floor a common-tree link path belongs to,
    /// for `prune_empty_ancestors`.
    fn common_tree_floor(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.root).ok().and_then(|relative| {
            relative
                .components()
                .next()
                .map(|component| component.as_os_str())
        }) {
            Some(top) => self.root.join(top),
            None => self.root.clone(),
        }
    }

    pub fn doctor(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let mut known_paths = HashSet::new();
        let mut statement = self
            .db
            .prepare("SELECT path,package_name,target FROM links ORDER BY path")?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (path, package, target) = row?;
            let actual = fs::read_link(self.root.join(&path));
            if actual.as_ref().ok() != Some(&PathBuf::from(&target)) {
                problems.push(format!(
                    "{path}: link owned by {package} is missing or points elsewhere"
                ));
            }
            known_paths.insert(path);
        }

        // A symlink into pkgs/ that the links table never recorded --
        // left over from an interrupted install/remove, or planted by
        // hand.
        let pkgs_root = self.root.join("pkgs");
        for tree in ["bin", "share"] {
            let base = self.root.join(tree);
            if !base.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&base) {
                let entry = entry?;
                if !entry.file_type().is_symlink() {
                    continue;
                }
                let Ok(relative) = entry.path().strip_prefix(&self.root) else {
                    continue;
                };
                let relative = relative.to_string_lossy().to_string();
                if known_paths.contains(&relative) {
                    continue;
                }
                if fs::read_link(entry.path()).is_ok_and(|target| target.starts_with(&pkgs_root)) {
                    problems.push(format!(
                        "{relative}: symlink into pkgs/ is not recorded in the link database"
                    ));
                }
            }
        }

        // Version directories under pkgs/ that don't match what's
        // recorded as current, and packages recorded as installed whose
        // payload directory is gone.
        let mut current_versions = HashMap::new();
        let mut package_statement = self.db.prepare("SELECT name,version FROM packages")?;
        for row in package_statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (name, version) = row?;
            current_versions.insert(name, version);
        }
        if pkgs_root.is_dir() {
            for namespace_entry in fs::read_dir(&pkgs_root)? {
                let namespace_entry = namespace_entry?;
                if !namespace_entry.file_type()?.is_dir() {
                    continue;
                }
                let namespace = namespace_entry.file_name().to_string_lossy().to_string();
                for name_entry in fs::read_dir(namespace_entry.path())? {
                    let name_entry = name_entry?;
                    if !name_entry.file_type()?.is_dir() {
                        continue;
                    }
                    let name = name_entry.file_name().to_string_lossy().to_string();
                    let full_name = format!("{namespace}/{name}");
                    let Some(current) = current_versions.get(&full_name) else {
                        problems.push(format!(
                            "pkgs/{full_name}: on disk but not recorded in the database"
                        ));
                        continue;
                    };
                    for version_entry in fs::read_dir(name_entry.path())? {
                        let version_entry = version_entry?;
                        if !version_entry.file_type()?.is_dir() {
                            continue;
                        }
                        let version = version_entry.file_name().to_string_lossy().to_string();
                        if &version != current {
                            problems.push(format!(
                                "pkgs/{full_name}/{version}: stale version directory, current is {current}"
                            ));
                        }
                    }
                }
            }
        }
        for (name, version) in &current_versions {
            let (namespace, package) = split_name(name)?;
            if !pkgs_root
                .join(namespace)
                .join(package)
                .join(version)
                .is_dir()
            {
                problems.push(format!(
                    "{name} {version}: recorded in the database but payload directory is missing"
                ));
            }
        }

        Ok(problems)
    }

    /// Restore only common-tree links that are recorded in YAM's state.
    /// Unowned files are never removed or replaced.
    pub fn repair(&mut self) -> Result<Vec<String>> {
        let _lock = self.lock_transaction()?;
        self.clear_stale_staging()?;
        let mut statement = self
            .db
            .prepare("SELECT path,target,package_name FROM links ORDER BY path")?;
        let links = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let payload_root = self.root.join("pkgs");
        let mut repaired = Vec::new();
        for (path, target, package_name) in links {
            validate_payload_path(&path)?;
            let link = self.root.join(&path);
            let target = PathBuf::from(target);
            if !target.starts_with(&payload_root) || !target.is_file() {
                bail!("cannot repair {path}: recorded payload is missing")
            }
            if fs::read_link(&link).ok().as_ref() == Some(&target) {
                continue;
            }
            if link.exists() && !link.is_symlink() {
                bail!("cannot repair {path}: it is not a YAM link")
            }
            if link.is_symlink() {
                let (namespace, name) = split_name(&package_name)?;
                let own_payload = payload_root.join(namespace).join(name);
                let current = fs::read_link(&link).ok();
                let safe_to_replace = current
                    .as_ref()
                    .is_none_or(|current| !current.exists() || current.starts_with(&own_payload));
                if !safe_to_replace {
                    bail!(
                        "cannot repair {path}: existing symlink points outside {}'s own payload ({})",
                        package_name,
                        describe_obstruction(&link)
                    )
                }
                fs::remove_file(&link)?;
            }
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent)?;
            }
            make_link(&link, &target)?;
            repaired.push(path);
        }
        // Whatever an interrupted transaction left behind, it's resolved
        // now: clear the marker so mutating commands can run again.
        let marker = self.transaction_marker_path();
        if marker.is_file() {
            fs::remove_file(&marker)?;
            repaired.push("db/transaction.json".to_string());
        }
        self.interrupted = false;
        Ok(repaired)
    }

    pub fn entware_available(&self) -> bool {
        self.profile
            .entware
            .as_ref()
            .is_some_and(|entry| entry.opkg.is_file())
    }

    pub fn entware_list(&self) -> Result<String> {
        let opkg = self
            .profile
            .entware
            .as_ref()
            .context("Entware is not configured for this platform")?;
        let output = Command::new(&opkg.opkg)
            .arg("list-installed")
            .output()
            .with_context(|| format!("run {}", opkg.opkg.display()))?;
        if !output.status.success() {
            bail!(
                "opkg list-installed failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        }
        Ok(String::from_utf8_lossy(&output.stdout).into())
    }

    fn ensure_entware(&self, manifest: &PackageManifest, yes: bool) -> Result<()> {
        let Some(request) = &manifest.entware else {
            return Ok(());
        };
        if request.packages.is_empty() {
            return Ok(());
        }
        let adapter = self
            .profile
            .entware
            .as_ref()
            .context("platform has no Entware adapter")?;
        if !adapter.opkg.is_file() {
            bail!(
                "Entware is not installed: {} is missing",
                adapter.opkg.display()
            )
        }
        if !yes {
            bail!("Entware package installation requires --yes")
        }
        let status = Command::new(&adapter.opkg)
            .arg("install")
            .args(&request.packages)
            .status()
            .with_context(|| format!("run {}", adapter.opkg.display()))?;
        if !status.success() {
            bail!("Entware package installation failed")
        }
        Ok(())
    }

    /// Validate `depends`/`conflicts`/`replaces` and return the full
    /// names of installed packages that must be removed because this
    /// install replaces them (A4). Does not touch the database or disk;
    /// callers run this before any mutation.
    fn plan_relations(&self, manifest: &PackageManifest) -> Result<Vec<String>> {
        for requirement in &manifest.depends {
            let (name, constraints) = parse_requirement(requirement)?;
            if !self.requirement_satisfied(name, &constraints)? {
                bail!(
                    "unsatisfied dependency {requirement} required by {}",
                    manifest.name
                )
            }
        }
        let mut to_replace: Vec<String> = Vec::new();
        for conflict in &manifest.conflicts {
            let (name, constraints) = parse_requirement(conflict)?;
            // A package never conflicts with itself, whether named
            // directly or reached through a virtual capability it
            // provides (e.g. upgrading in place while conflicting with
            // its own `provides`).
            let matches: Vec<String> = self
                .satisfying_packages(name, &constraints)?
                .into_iter()
                .filter(|package_name| package_name != &manifest.name)
                .collect();
            if matches.is_empty() {
                continue;
            }
            let covering_replaces: Vec<&String> = manifest
                .replaces
                .iter()
                .filter(|entry| {
                    requirement_name(entry).is_ok_and(|replaced_name| replaced_name == name)
                })
                .collect();
            if covering_replaces.is_empty() {
                bail!(
                    "{} conflicts with installed package or capability {conflict}",
                    manifest.name
                )
            }
            for package_name in &matches {
                let installed_version = self.installed_version(package_name)?;
                let covered = covering_replaces.iter().any(|entry| {
                    parse_requirement(entry)
                        .is_ok_and(|(_, limits)| limits.matches(&installed_version))
                });
                if !covered {
                    bail!(
                        "{} conflicts with installed {package_name}, and replaces {conflict} \
                         does not cover its installed version {installed_version}",
                        manifest.name
                    )
                }
                if !to_replace.contains(package_name) {
                    to_replace.push(package_name.clone());
                }
            }
        }
        self.check_replace_dependents(&to_replace, manifest)?;
        Ok(to_replace)
    }

    /// Refuse the replace if some other installed package still needs
    /// one of `to_replace`: by its exact full name (always a hard
    /// block), or by a virtual capability no other installed package
    /// and no part of `manifest` itself would cover once they're gone.
    fn check_replace_dependents(
        &self,
        to_replace: &[String],
        manifest: &PackageManifest,
    ) -> Result<()> {
        if to_replace.is_empty() {
            return Ok(());
        }
        let mut statement = self
            .db
            .prepare("SELECT package_name, requirement FROM dependencies")?;
        let rows: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut broken = Vec::new();
        for (dependent, requirement) in rows {
            if to_replace.contains(&dependent) || dependent == manifest.name {
                continue;
            }
            let (required_name, required_constraints) = parse_requirement(&requirement)?;
            if to_replace.iter().any(|replaced| replaced == required_name) {
                broken.push(format!("{dependent} ({requirement})"));
                continue;
            }
            let remaining_providers = self
                .satisfying_packages(required_name, &required_constraints)?
                .into_iter()
                .any(|provider| !to_replace.contains(&provider));
            if remaining_providers {
                continue;
            }
            let new_version = version::parse_version(&manifest.version)?;
            let new_covers = manifest.provides.iter().any(|p| p == required_name)
                && required_constraints.matches(&new_version);
            if !new_covers {
                broken.push(format!("{dependent} ({requirement})"));
            }
        }
        if !broken.is_empty() {
            bail!(
                "cannot replace {}; required by {}",
                to_replace.join(", "),
                broken.join(", ")
            )
        }
        Ok(())
    }

    fn installed_version(&self, name: &str) -> Result<semver::Version> {
        let version: String = self
            .db
            .query_row(
                "SELECT version FROM packages WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .with_context(|| format!("{name} is not installed"))?;
        version::parse_version(&version)
    }

    fn requirement_satisfied(
        &self,
        name: &str,
        constraints: &version::Constraints,
    ) -> Result<bool> {
        Ok(!self.satisfying_packages(name, constraints)?.is_empty())
    }

    /// Every installed package name that is itself called `name`, or
    /// that provides the virtual capability `name`, whose installed
    /// version matches `constraints`.
    fn satisfying_packages(
        &self,
        name: &str,
        constraints: &version::Constraints,
    ) -> Result<Vec<String>> {
        let mut statement = self.db.prepare(
            "SELECT p.name,p.version FROM packages p WHERE p.name=?1
             UNION SELECT p.name,p.version FROM packages p JOIN provides v ON v.package_name=p.name WHERE v.capability=?1",
        )?;
        let candidates = statement
            .query_map([name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut matched = Vec::new();
        for (package_name, candidate_version) in candidates {
            let candidate_version = version::parse_version(&candidate_version)?;
            if constraints.matches(&candidate_version) {
                matched.push(package_name);
            }
        }
        Ok(matched)
    }
}
