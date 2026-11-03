use super::*;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn payload_modes_preserve_execute_intent_but_not_privileged_or_writable_bits() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let file = work.path().join("payload");
    fs::write(&file, b"test").unwrap();
    for (path, archive_mode, expected) in [
        ("share/example.tool/bin/private", 0o755, 0o755),
        ("share/example.tool/lib/loader.so", 0o711, 0o755),
        ("share/example.tool/run.sh", 0o7777, 0o755),
        ("share/example.tool/config.json", 0o6666, 0o644),
        ("share/example.tool/readme", 0o600, 0o644),
        ("bin/legacy-tool", 0o644, 0o755),
        ("hooks/legacy-hook", 0o644, 0o755),
    ] {
        set_payload_mode(&file, path, archive_mode).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            expected,
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn packed_private_executable_runs_after_install_without_postinstall_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let profile = test_profile(work.path());
    let source = work.path().join("source");
    fs::create_dir_all(source.join("payload/bin")).unwrap();
    fs::create_dir_all(source.join("payload/lib")).unwrap();
    fs::write(source.join("yam.yaml"), "name: example/tool\nversion: 1.0.0\nlicense: MIT\ndescription: Private executable fixture\ntargets: [testplatform]\nfiles:\n  share/example.tool/: payload/\n").unwrap();
    fs::write(
        source.join("payload/bin/private-tool"),
        "#!/bin/sh\nprintf '%s\\n' private-tool-ok\n",
    )
    .unwrap();
    fs::set_permissions(
        source.join("payload/bin/private-tool"),
        fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    fs::write(
        source.join("payload/lib/private-loader.so"),
        b"non-executed loader fixture",
    )
    .unwrap();
    fs::set_permissions(
        source.join("payload/lib/private-loader.so"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(source.join("payload/config.json"), b"{}").unwrap();
    fs::set_permissions(
        source.join("payload/config.json"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let artifact = work.path().join("tool.yampkg");
    Manager::pack(&source, &profile, &artifact).unwrap();
    let root = profile.paths.root.clone();
    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();
    let program = root.join("share/example.tool/bin/private-tool");
    assert_eq!(
        fs::metadata(&program).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        fs::metadata(root.join("share/example.tool/lib/private-loader.so"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    assert_eq!(
        fs::metadata(root.join("share/example.tool/config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
    let output = Command::new(&program).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .starts_with("private-tool-ok"));
    assert_eq!(
        manager
            .which("share/example.tool/bin/private-tool")
            .unwrap(),
        Some("example/tool".into())
    );
    assert!(manager.doctor().unwrap().is_empty());
    manager.remove("example/tool").unwrap();
    assert!(!program.exists());
}

#[test]
fn package_names_and_payload_paths_are_strict() {
    assert!(validate_name("nikita/soloist").is_ok());
    assert!(validate_name("soloist").is_err());
    assert!(validate_payload_path("share/host/a.lua").is_ok());
    assert!(validate_payload_path("../etc/passwd").is_err());
}

/// A platform profile whose every path lives under a fresh temp dir,
/// so tests never touch a real filesystem location or each other.
fn test_profile(workdir: &Path) -> PlatformProfile {
    serde_yaml::from_str(&format!(
            "id: testplatform\narch: aarch64\ntriple: aarch64-unknown-linux-musl\npaths:\n  root: {:?}\n  etc: {:?}\n  var: {:?}\n  run: {:?}\n",
            workdir.join("root"),
            workdir.join("etc"),
            workdir.join("var"),
            workdir.join("run"),
        ))
        .expect("inline test profile must parse")
}

/// Builder for a `yam.yaml` source tree, packed through `Manager::pack`
/// exactly as a real package author would. Shared by every test below
/// that needs an installable `.yampkg`.
struct TestPackage {
    name: String,
    version: String,
    files: Vec<(String, Vec<u8>)>,
    depends: Vec<String>,
    provides: Vec<String>,
    conflicts: Vec<String>,
    replaces: Vec<String>,
    postinstall: Vec<String>,
}
impl TestPackage {
    fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            files: Vec::new(),
            depends: Vec::new(),
            provides: Vec::new(),
            conflicts: Vec::new(),
            replaces: Vec::new(),
            postinstall: Vec::new(),
        }
    }
    fn file(mut self, path: &str, content: &[u8]) -> Self {
        self.files.push((path.into(), content.to_vec()));
        self
    }
    #[allow(dead_code)]
    fn depends(mut self, value: &str) -> Self {
        self.depends.push(value.into());
        self
    }
    #[allow(dead_code)]
    fn provides(mut self, value: &str) -> Self {
        self.provides.push(value.into());
        self
    }
    #[allow(dead_code)]
    fn conflicts(mut self, value: &str) -> Self {
        self.conflicts.push(value.into());
        self
    }
    #[allow(dead_code)]
    fn replaces(mut self, value: &str) -> Self {
        self.replaces.push(value.into());
        self
    }
    #[allow(dead_code)]
    fn postinstall(mut self, command: &[&str]) -> Self {
        self.postinstall = command.iter().map(|value| (*value).into()).collect();
        self
    }

    /// Write the source tree under `source_dir` and pack it into `output`.
    fn pack(&self, source_dir: &Path, profile: &PlatformProfile, output: &Path) -> Result<()> {
        fs::create_dir_all(source_dir)?;
        let mut files_entry = String::new();
        for (path, _) in &self.files {
            files_entry += &format!("  {path}: {path}\n");
        }
        let yaml = format!(
                "name: {}\nversion: {}\nlicense: MIT\ndescription: test package\ntargets: [any]\nfiles:\n{}depends: [{}]\nprovides: [{}]\nconflicts: [{}]\nreplaces: [{}]\npostinstall: [{}]\n",
                self.name,
                self.version,
                files_entry,
                self.depends.join(", "),
                self.provides.join(", "),
                self.conflicts.join(", "),
                self.replaces.join(", "),
                self.postinstall.join(", "),
            );
        fs::write(source_dir.join("yam.yaml"), yaml)?;
        for (path, content) in &self.files {
            let full = source_dir.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(full, content)?;
        }
        Manager::pack(source_dir, profile, output)
    }
}

/// A minimal, directly-constructed manifest, for tests that need to
/// write an archive bypassing `Manager::pack` (malicious/malformed
/// archives: A3, B4).
fn manifest_for(
    name: &str,
    version: &str,
    target: &str,
    arch: &str,
    files: &[(&str, &[u8])],
) -> PackageManifest {
    PackageManifest {
        name: name.into(),
        version: version.into(),
        license: "MIT".into(),
        description: "test package".into(),
        homepage: None,
        target: target.into(),
        arch: arch.into(),
        depends: Vec::new(),
        recommends: Vec::new(),
        enhances: Vec::new(),
        provides: Vec::new(),
        conflicts: Vec::new(),
        replaces: Vec::new(),
        permissions: Vec::new(),
        entware: None,
        services: BTreeMap::new(),
        postinstall: Vec::new(),
        files_sha256: files
            .iter()
            .map(|(path, content)| (path.to_string(), digest_bytes(content)))
            .collect(),
    }
}

/// Build a `.yampkg` by hand (MANIFEST.yaml + files/*), bypassing every
/// check `Manager::pack` performs on a well-behaved source tree.
fn build_raw_archive(
    output: &Path,
    manifest: &PackageManifest,
    files: &[(&str, &[u8])],
) -> Result<()> {
    let file = fs::File::create(output)?;
    let encoder = zstd::Encoder::new(file, 1)?;
    let mut archive = Builder::new(encoder.auto_finish());
    append_bytes(
        &mut archive,
        "MANIFEST.yaml",
        serde_yaml::to_string(manifest)?.as_bytes(),
    )?;
    for (path, content) in files {
        append_bytes(&mut archive, &format!("files/{path}"), content)?;
    }
    archive.finish()?;
    Ok(())
}

/// Everything a test should care about having changed or not: every
/// installed package's recorded files (via the public API), plus the
/// whole file tree under `root` (symlink targets, directories, and
/// regular file contents by hash), excluding `db/` (its raw bytes
/// legitimately differ run to run even with identical logical state)
/// and `.staging/` (scratch space). Used by B1 tests to assert a
/// failed transaction left absolutely nothing behind.
#[derive(Debug, PartialEq, Eq)]
struct RootSnapshot {
    packages: Vec<(String, String, Vec<String>)>,
    tree: BTreeMap<String, String>,
}
fn snapshot_root(manager: &Manager, root: &Path) -> RootSnapshot {
    let packages = manager
        .list()
        .unwrap()
        .into_iter()
        .map(|package| {
            let files = manager.files(&package.name).unwrap();
            (package.name, package.version, files)
        })
        .collect();
    let mut tree = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.path() == root {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if relative == "db" || relative.starts_with("db/") || relative == ".staging" {
            continue;
        }
        let kind = if entry.file_type().is_symlink() {
            format!("symlink:{}", fs::read_link(entry.path()).unwrap().display())
        } else if entry.file_type().is_dir() {
            "dir".to_string()
        } else {
            format!("file:{}", digest_file(entry.path()).unwrap())
        };
        tree.insert(relative, kind);
    }
    RootSnapshot { packages, tree }
}

#[test]
fn install_refuses_to_overwrite_a_foreign_regular_file() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/busybox-tool", b"yam payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let root = profile.paths.root.clone();
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/busybox-tool"), b"not yam's file").unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    match manager.install(&artifact, true) {
        Ok(_) => {
            panic!("install must refuse to overwrite the foreign file at bin/busybox-tool")
        }
        Err(error) => assert!(
            error.to_string().contains("bin/busybox-tool"),
            "error should name the offending path, was: {error}"
        ),
    }

    assert_eq!(
        fs::read(root.join("bin/busybox-tool")).unwrap(),
        b"not yam's file"
    );
    assert!(manager.list().unwrap().is_empty());
    assert!(!root.join("pkgs/vasya/plug").exists());
}

#[test]
fn install_refuses_to_overwrite_a_foreign_symlink() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/busybox-tool", b"yam payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let root = profile.paths.root.clone();
    fs::create_dir_all(root.join("bin")).unwrap();
    let elsewhere = workdir.path().join("elsewhere");
    fs::write(&elsewhere, b"not managed by yam").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, root.join("bin/busybox-tool")).unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    match manager.install(&artifact, true) {
        Ok(_) => {
            panic!("install must refuse to replace the foreign symlink at bin/busybox-tool")
        }
        Err(error) => assert!(
            error.to_string().contains("bin/busybox-tool"),
            "error should name the offending path, was: {error}"
        ),
    }

    assert_eq!(
        fs::read_link(root.join("bin/busybox-tool")).unwrap(),
        elsewhere
    );
    assert!(manager.list().unwrap().is_empty());
}

#[test]
fn repair_refuses_to_replace_a_symlink_pointing_outside_its_own_package() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"yam payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let root = profile.paths.root.clone();
    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();

    let link = root.join("bin/plug-tool");
    let foreign_target = workdir.path().join("foreign-binary");
    fs::write(&foreign_target, b"not yam's payload").unwrap();
    fs::remove_file(&link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&foreign_target, &link).unwrap();

    let result = manager.repair();
    assert!(
        result.is_err(),
        "repair must refuse to replace a foreign symlink, got: {result:?}"
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        foreign_target,
        "the foreign symlink must be left untouched"
    );
}

#[test]
fn pack_rejects_a_share_path_missing_the_namespace_prefix() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let artifact = workdir.path().join("bad.yampkg");
    let result = TestPackage::new("vasya/bad", "1.0.0")
        .file("share/host/plugins/main.lua", b"return {}")
        .pack(&workdir.path().join("src"), &profile, &artifact);
    assert!(
        result.is_err(),
        "pack must reject a share/ path without the vasya. prefix, got {result:?}"
    );
    assert!(!artifact.exists());
}

#[test]
fn install_rejects_an_archive_with_a_share_path_missing_the_namespace_prefix() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let files: [(&str, &[u8]); 1] = [("share/host/plugins/main.lua", b"return {}")];
    let manifest = manifest_for("vasya/bad", "1.0.0", "any", &profile.arch, &files);
    let artifact = workdir.path().join("bad.yampkg");
    build_raw_archive(&artifact, &manifest, &files).unwrap();

    let root = profile.paths.root.clone();
    let mut manager = Manager::open(root, profile).unwrap();
    let result = manager.install(&artifact, true);
    assert!(
            result.is_err(),
            "install must reject a share/ path without the namespace prefix even when pack was bypassed, got {result:?}"
        );
    assert!(manager.list().unwrap().is_empty());
}

#[test]
fn installing_a_package_that_declares_a_dependency_succeeds() {
    // Found while writing the B5 test below, not part of the original
    // A1-B6 list: install_payload inserts into `dependencies` (and
    // `provides`/`services`) before it inserts the package's own row
    // into `packages`, even on a first-time install. Those tables'
    // `package_name` columns are `REFERENCES packages(name)` and
    // foreign_keys is on, so SQLite rejects the dependency row before
    // the package row exists.
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let base_artifact = workdir.path().join("base.yampkg");
    TestPackage::new("yam/base", "1.0.0")
        .file("bin/base-tool", b"base payload")
        .pack(&workdir.path().join("base-src"), &profile, &base_artifact)
        .unwrap();

    let app_artifact = workdir.path().join("app.yampkg");
    TestPackage::new("yam/app", "1.0.0")
        .file("bin/app-tool", b"app payload")
        .depends("yam/base >=1.0.0")
        .pack(&workdir.path().join("app-src"), &profile, &app_artifact)
        .unwrap();

    let mut manager = Manager::open(root, profile).unwrap();
    manager.install(&base_artifact, true).unwrap();
    manager
        .install(&app_artifact, true)
        .expect("installing a package with a satisfied dependency must succeed");
}

#[test]
fn postinstall_runs_with_the_documented_environment() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();
    let artifact = workdir.path().join("hook.yampkg");
    TestPackage::new("yam/hook", "1.0.0")
        .file(
            "bin/postinstall",
            b"#!/bin/sh\nmkdir -p \"$YAM_VAR\"\nprintf '%s|%s|%s' \"$YAM_PLATFORM\" \"$YAM_ROOT\" \"$YAM_PKG\" > \"$YAM_VAR/postinstall-result\"\n",
        )
        .postinstall(&["bin/postinstall"])
        .pack(&workdir.path().join("hook-src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile.clone()).unwrap();
    manager.install(&artifact, true).unwrap();

    assert_eq!(
        fs::read_to_string(profile.paths.var.join("postinstall-result")).unwrap(),
        format!(
            "testplatform|{}|{}",
            root.display(),
            root.join("pkgs/yam/hook/1.0.0").display()
        )
    );
}

#[test]
fn failed_postinstall_rolls_back_package_state() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();
    let artifact = workdir.path().join("failing-hook.yampkg");
    TestPackage::new("yam/failing-hook", "1.0.0")
        .file("bin/postinstall", b"#!/bin/sh\nexit 23\n")
        .file("bin/hook-tool", b"payload")
        .postinstall(&["bin/postinstall"])
        .pack(
            &workdir.path().join("failing-hook-src"),
            &profile,
            &artifact,
        )
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    let result = manager.install(&artifact, true);
    assert!(
        result.is_err(),
        "a failing postinstall must fail installation"
    );
    assert!(result.unwrap_err().to_string().contains("postinstall"));
    assert!(manager.list().unwrap().is_empty());
    assert!(!root.join("bin/hook-tool").exists());
    assert!(!root.join("pkgs/yam/failing-hook").exists());
    assert!(!root.join("db/transaction.json").exists());
}

#[test]
fn a_prerelease_never_implicitly_satisfies_a_stable_at_least_constraint() {
    // core.rs's own `Version` derives Ord over (major, minor, patch,
    // pre: Option<String>). Option's derived Ord puts `None` below
    // `Some(_)`, so "1.5.0" (pre: None) compares as LESS than
    // "1.5.0-rc.1" (pre: Some("rc.1")) -- backwards from SemVer,
    // where a release must sort above its own pre-releases. That
    // makes `AtLeast(1.5.0)` (">=1.5.0") wrongly match an installed
    // "1.5.0-rc.1".
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let base_artifact = workdir.path().join("base.yampkg");
    TestPackage::new("yam/base", "1.5.0-rc.1")
        .file("bin/base-tool", b"base payload")
        .pack(&workdir.path().join("base-src"), &profile, &base_artifact)
        .unwrap();

    let app_artifact = workdir.path().join("app.yampkg");
    TestPackage::new("yam/app", "1.0.0")
        .file("bin/app-tool", b"app payload")
        .depends("yam/base >=1.5.0")
        .pack(&workdir.path().join("app-src"), &profile, &app_artifact)
        .unwrap();

    let mut manager = Manager::open(root, profile).unwrap();
    manager.install(&base_artifact, true).unwrap();

    let result = manager.install(&app_artifact, true);
    assert!(
        result.is_err(),
        "a pre-release (1.5.0-rc.1) must never satisfy >=1.5.0, got {result:?}"
    );
}

#[test]
fn upgrade_removes_stale_links_and_old_version_directory() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let v1 = workdir.path().join("plug-1.0.0.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("share/host/plugins/vasya.plug/main.lua", b"v1 main")
        .file("share/host/plugins/vasya.plug/old.lua", b"v1 old")
        .pack(&workdir.path().join("src-1"), &profile, &v1)
        .unwrap();

    let v2 = workdir.path().join("plug-1.1.0.yampkg");
    TestPackage::new("vasya/plug", "1.1.0")
        .file("share/host/plugins/vasya.plug/main.lua", b"v2 main")
        .pack(&workdir.path().join("src-2"), &profile, &v2)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&v1, true).unwrap();
    manager.install(&v2, true).unwrap();

    let old_lua = root.join("share/host/plugins/vasya.plug/old.lua");
    assert!(
        !old_lua.exists() && !old_lua.is_symlink(),
        "old.lua from 1.0.0 must be gone after upgrading to 1.1.0"
    );
    assert!(
        !root.join("pkgs/vasya/plug/1.0.0").exists(),
        "the old version's payload directory must be removed after upgrade"
    );
    assert_eq!(
        manager.files("vasya/plug").unwrap(),
        vec!["share/host/plugins/vasya.plug/main.lua".to_string()]
    );
    assert!(
        manager.doctor().unwrap().is_empty(),
        "doctor must have nothing left to report once the stale link is actually gone"
    );
}

#[test]
fn remove_deletes_every_version_and_prunes_now_empty_directories() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let v1 = workdir.path().join("plug-1.0.0.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("share/host/plugins/vasya.plug/main.lua", b"v1 main")
        .file("share/host/plugins/vasya.plug/old.lua", b"v1 old")
        .pack(&workdir.path().join("src-1"), &profile, &v1)
        .unwrap();
    let v2 = workdir.path().join("plug-1.1.0.yampkg");
    TestPackage::new("vasya/plug", "1.1.0")
        .file("share/host/plugins/vasya.plug/main.lua", b"v2 main")
        .pack(&workdir.path().join("src-2"), &profile, &v2)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&v1, true).unwrap();
    manager.install(&v2, true).unwrap();
    manager.remove("vasya/plug").unwrap();

    assert!(manager.list().unwrap().is_empty());
    assert!(
        !root.join("pkgs/vasya/plug").exists(),
        "every version directory must be gone after remove"
    );
    assert!(
        !root.join("share/host/plugins/vasya.plug").exists(),
        "the now-empty package-specific share/ directory must be pruned"
    );
}

#[test]
fn remove_does_not_delete_a_foreign_file_left_in_a_shared_directory() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("share/host/plugins/vasya.plug/main.lua", b"main")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();

    let foreign = root.join("share/host/plugins/vasya.plug/notes.txt");
    fs::write(&foreign, b"not part of the package").unwrap();

    manager.remove("vasya/plug").unwrap();

    assert!(
        foreign.exists(),
        "remove must not delete a foreign file sharing the package's directory"
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"not part of the package");
}

#[test]
fn doctor_detects_an_untracked_symlink_into_pkgs() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();

    // Planted by hand, bypassing the manager entirely: a symlink into
    // pkgs/ that the links table never recorded.
    let rogue = root.join("bin/rogue-tool");
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("pkgs/vasya/plug/1.0.0/bin/plug-tool"), &rogue).unwrap();

    let problems = manager.doctor().unwrap();
    assert!(
        problems.iter().any(|p| p.contains("bin/rogue-tool")),
        "doctor must report an untracked symlink into pkgs/, got {problems:?}"
    );
}

#[test]
fn doctor_detects_a_stale_version_directory() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();

    // Planted by hand: a leftover version directory that is not the
    // one recorded as current (as if a crash interrupted cleanup).
    let stale = root.join("pkgs/vasya/plug/0.9.0");
    fs::create_dir_all(&stale).unwrap();

    let problems = manager.doctor().unwrap();
    assert!(
        problems.iter().any(|p| p.contains("0.9.0")),
        "doctor must report a stale version directory, got {problems:?}"
    );
}

#[test]
fn doctor_detects_a_package_missing_its_payload_directory() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();

    fs::remove_dir_all(root.join("pkgs/vasya/plug/1.0.0")).unwrap();

    let problems = manager.doctor().unwrap();
    assert!(
        problems.iter().any(|p| p.contains("vasya/plug")),
        "doctor must report a package whose payload directory is gone, got {problems:?}"
    );
}

#[test]
fn install_failure_mid_transaction_leaves_everything_unchanged() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/a-tool", b"a")
        .file("bin/b-tool", b"b")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    let before = snapshot_root(&manager, &root);

    manager.inject_link_failure_after(1);
    let result = manager.install(&artifact, true);
    assert!(
        result.is_err(),
        "the injected fault must propagate as an error"
    );

    let after = snapshot_root(&manager, &root);
    assert_eq!(
        before, after,
        "disk and database must be exactly as they were before a failed install"
    );

    drop(manager);
    let mut retry = Manager::open(root.clone(), test_profile(workdir.path())).unwrap();
    retry
        .install(&artifact, true)
        .expect("installing the same package again after a rolled-back failure must succeed");
}

#[test]
fn upgrade_failure_mid_transaction_keeps_the_old_version_active() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let v1 = workdir.path().join("v1.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"v1")
        .pack(&workdir.path().join("src-1"), &profile, &v1)
        .unwrap();
    let v2 = workdir.path().join("v2.yampkg");
    TestPackage::new("vasya/plug", "1.1.0")
        .file("bin/plug-tool", b"v2")
        .file("bin/extra-tool", b"extra")
        .pack(&workdir.path().join("src-2"), &profile, &v2)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&v1, true).unwrap();
    let before = snapshot_root(&manager, &root);

    manager.inject_link_failure_after(1);
    let result = manager.install(&v2, true);
    assert!(
        result.is_err(),
        "the injected fault must propagate as an error"
    );

    let after = snapshot_root(&manager, &root);
    assert_eq!(
        before, after,
        "a failed upgrade must leave the old version's disk and database state untouched"
    );
    assert_eq!(
        fs::read(root.join("bin/plug-tool")).unwrap(),
        b"v1",
        "the old version's binary must still be the one served"
    );
}

#[test]
fn remove_failure_mid_transaction_leaves_the_package_fully_installed() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/a-tool", b"a")
        .file("bin/b-tool", b"b")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&artifact, true).unwrap();
    let before = snapshot_root(&manager, &root);

    manager.inject_remove_failure_after(1);
    let result = manager.remove("vasya/plug");
    assert!(
        result.is_err(),
        "the injected fault must propagate as an error"
    );

    let after = snapshot_root(&manager, &root);
    assert_eq!(
        before, after,
        "a failed remove must leave the package exactly as installed, including every link"
    );
}

#[test]
fn mutating_commands_refuse_to_run_after_an_interrupted_transaction() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    // Simulate a crash: a transaction marker left behind because the
    // process died before clearing it, as Manager::open would find
    // on the next run.
    let manager = Manager::open(root.clone(), profile).unwrap();
    fs::write(root.join("db/transaction.json"), b"{}").unwrap();
    drop(manager);

    let mut reopened = Manager::open(root.clone(), test_profile(workdir.path())).unwrap();
    let result = reopened.install(&artifact, true);
    assert!(
        result.is_err(),
        "install must refuse to run while an interrupted transaction's marker is present"
    );

    reopened.repair().unwrap();
    reopened
        .install(&artifact, true)
        .expect("install must work again once repair has cleared the interrupted marker");
}

#[test]
fn repair_cleans_up_stale_staging_directories() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();
    let mut manager = Manager::open(root.clone(), profile).unwrap();

    let stale = root.join(".staging").join("leftover-from-a-crash");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("partial.bin"), b"partial").unwrap();

    manager.repair().unwrap();

    assert!(
        !stale.exists(),
        "repair must clear stale .staging/ directories left by an interrupted transaction"
    );
}

#[test]
fn install_refuses_to_run_while_another_transaction_holds_the_lock() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();
    let run_dir = profile.paths.run.clone();

    let artifact = workdir.path().join("plug.yampkg");
    TestPackage::new("vasya/plug", "1.0.0")
        .file("bin/plug-tool", b"payload")
        .pack(&workdir.path().join("src"), &profile, &artifact)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    let before = snapshot_root(&manager, &root);

    // Simulate another yam process already holding the transaction
    // lock.
    fs::create_dir_all(&run_dir).unwrap();
    let held = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(run_dir.join("yam.lock"))
        .unwrap();
    fs4::fs_std::FileExt::lock_exclusive(&held).unwrap();

    let result = manager.install(&artifact, true);
    assert!(
        result.is_err(),
        "install must refuse to run while another transaction holds the lock, got {result:?}"
    );

    drop(held);

    let after = snapshot_root(&manager, &root);
    assert_eq!(
        before, after,
        "a refused install due to a held lock must change nothing on disk or in the database"
    );
}

#[test]
fn install_rejects_an_archive_with_too_many_entries() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let files: Vec<(String, Vec<u8>)> = (0..20)
        .map(|index| (format!("bin/tool-{index}"), b"x".to_vec()))
        .collect();
    let file_refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_slice()))
        .collect();
    let manifest = manifest_for("vasya/plug", "1.0.0", "any", &profile.arch, &file_refs);
    let artifact = workdir.path().join("plug.yampkg");
    build_raw_archive(&artifact, &manifest, &file_refs).unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.inject_archive_limits(ArchiveLimits {
        max_entries: 5,
        ..ArchiveLimits::DEFAULT
    });
    let result = manager.install(&artifact, true);
    assert!(
        result.is_err(),
        "install must reject an archive with more entries than the limit, got {result:?}"
    );
    assert!(
        fs::read_dir(root.join(".staging"))
            .unwrap()
            .next()
            .is_none(),
        ".staging must be clean after a rejected archive"
    );
}

#[test]
fn install_rejects_an_archive_exceeding_the_unpacked_size_limit() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let big_content = vec![b'x'; 4096];
    let files: [(&str, &[u8]); 1] = [("bin/big-tool", &big_content)];
    let manifest = manifest_for("vasya/plug", "1.0.0", "any", &profile.arch, &files);
    let artifact = workdir.path().join("plug.yampkg");
    build_raw_archive(&artifact, &manifest, &files).unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.inject_archive_limits(ArchiveLimits {
        max_total_unpacked_bytes: 1024,
        max_file_bytes: 1024,
        ..ArchiveLimits::DEFAULT
    });
    let result = manager.install(&artifact, true);
    assert!(
        result.is_err(),
        "install must reject an archive exceeding the unpacked size limit, got {result:?}"
    );
    assert!(
        fs::read_dir(root.join(".staging"))
            .unwrap()
            .next()
            .is_none(),
        ".staging must be clean after a rejected archive"
    );
}

#[test]
fn install_rejects_a_conflicting_package_without_replaces() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let soloist = workdir.path().join("soloist.yampkg");
    TestPackage::new("yam/soloist", "1.0.0")
        .file("bin/soloist-tool", b"soloist")
        .provides("spotify-connect")
        .pack(&workdir.path().join("soloist-src"), &profile, &soloist)
        .unwrap();

    let librespot = workdir.path().join("librespot.yampkg");
    TestPackage::new("vasya/librespot", "1.0.0")
        .file("bin/librespot-tool", b"librespot")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .pack(&workdir.path().join("librespot-src"), &profile, &librespot)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&soloist, true).unwrap();

    let result = manager.install(&librespot, true);
    assert!(
        result.is_err(),
        "a conflict with no replaces must be rejected, got {result:?}"
    );
    assert!(manager
        .list()
        .unwrap()
        .iter()
        .any(|p| p.name == "yam/soloist"));
}

#[test]
fn upgrading_a_package_that_conflicts_with_its_own_virtual_name_succeeds() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let v1 = workdir.path().join("v1.yampkg");
    TestPackage::new("yam/soloist", "1.0.0")
        .file("bin/soloist-tool", b"v1")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .pack(&workdir.path().join("src-1"), &profile, &v1)
        .unwrap();
    let v2 = workdir.path().join("v2.yampkg");
    TestPackage::new("yam/soloist", "1.1.0")
        .file("bin/soloist-tool", b"v2")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .pack(&workdir.path().join("src-2"), &profile, &v2)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&v1, true).unwrap();
    manager.install(&v2, true).expect(
        "upgrading a package that conflicts with its own provided virtual name \
             must not be treated as a self-conflict",
    );
}

#[test]
fn install_with_replaces_removes_the_conflicting_package_and_keeps_dependents_satisfied() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let soloist = workdir.path().join("soloist.yampkg");
    TestPackage::new("yam/soloist", "1.0.0")
        .file("bin/soloist-tool", b"soloist")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .replaces("spotify-connect")
        .pack(&workdir.path().join("soloist-src"), &profile, &soloist)
        .unwrap();

    let led = workdir.path().join("led.yampkg");
    TestPackage::new("petya/led", "1.0.0")
        .file("bin/led-tool", b"led")
        .depends("spotify-connect")
        .pack(&workdir.path().join("led-src"), &profile, &led)
        .unwrap();

    let librespot = workdir.path().join("librespot.yampkg");
    TestPackage::new("vasya/librespot", "1.0.0")
        .file("bin/librespot-tool", b"librespot")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .replaces("spotify-connect")
        .pack(&workdir.path().join("librespot-src"), &profile, &librespot)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&soloist, true).unwrap();
    manager.install(&led, true).unwrap();

    manager
        .install(&librespot, true)
        .expect("installing an explicit replacement for a conflicting provider must succeed");

    let installed: Vec<String> = manager
        .list()
        .unwrap()
        .into_iter()
        .map(|package| package.name)
        .collect();
    assert!(
        !installed.contains(&"yam/soloist".to_string()),
        "the replaced package must be gone: {installed:?}"
    );
    assert!(installed.contains(&"vasya/librespot".to_string()));
    assert!(
        installed.contains(&"petya/led".to_string()),
        "a dependent satisfied through the replaced virtual capability must remain installed"
    );
    assert!(
        !root.join("pkgs/yam/soloist").exists(),
        "the replaced package's payload directory must be gone"
    );
}

#[test]
fn install_rejects_replacing_a_package_an_exact_name_dependency_still_needs() {
    let workdir = TempDir::new().unwrap();
    let profile = test_profile(workdir.path());
    let root = profile.paths.root.clone();

    let soloist = workdir.path().join("soloist.yampkg");
    TestPackage::new("yam/soloist", "1.0.0")
        .file("bin/soloist-tool", b"soloist")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .replaces("spotify-connect")
        .pack(&workdir.path().join("soloist-src"), &profile, &soloist)
        .unwrap();

    let pinned = workdir.path().join("pinned.yampkg");
    TestPackage::new("petya/pinned", "1.0.0")
        .file("bin/pinned-tool", b"pinned")
        .depends("yam/soloist")
        .pack(&workdir.path().join("pinned-src"), &profile, &pinned)
        .unwrap();

    let librespot = workdir.path().join("librespot.yampkg");
    TestPackage::new("vasya/librespot", "1.0.0")
        .file("bin/librespot-tool", b"librespot")
        .provides("spotify-connect")
        .conflicts("spotify-connect")
        .replaces("spotify-connect")
        .pack(&workdir.path().join("librespot-src"), &profile, &librespot)
        .unwrap();

    let mut manager = Manager::open(root.clone(), profile).unwrap();
    manager.install(&soloist, true).unwrap();
    manager.install(&pinned, true).unwrap();

    let result = manager.install(&librespot, true);
    assert!(
        result.is_err(),
        "replacing a package an exact-name dependency still needs must fail, got {result:?}"
    );
    assert!(
        manager
            .list()
            .unwrap()
            .iter()
            .any(|p| p.name == "yam/soloist"),
        "yam/soloist must remain installed after the rejected replace"
    );
}

#[test]
fn alias_target_package_installs_under_the_device_profile() {
    let work = TempDir::new().unwrap();
    let paths = format!(
        "paths:\n  root: {:?}\n  etc: {:?}\n  var: {:?}\n  run: {:?}\n",
        work.path().join("root"),
        work.path().join("etc"),
        work.path().join("var"),
        work.path().join("run"),
    );
    let plum: PlatformProfile = serde_yaml::from_str(&format!(
        "id: plum\narch: aarch64\ntriple: aarch64-unknown-linux-musl\n{paths}"
    ))
    .unwrap();

    let source = work.path().join("alias-src");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("yam.yaml"),
        "name: example/alias\nversion: 1.0.0\nlicense: MIT\ndescription: alias fixture\ntargets: [plum]\nfiles:\n  share/example.txt: example.txt\n",
    )
    .unwrap();
    fs::write(source.join("example.txt"), b"alias").unwrap();
    let artifact = work.path().join("alias.yampkg");
    Manager::pack(&source, &plum, &artifact).unwrap();

    let yandexplum: PlatformProfile = serde_yaml::from_str(&format!(
        "id: yandexplum\naliases: [plum]\narch: aarch64\ntriple: aarch64-unknown-linux-musl\n{paths}"
    ))
    .unwrap();
    let mut manager = Manager::open(yandexplum.paths.root.clone(), yandexplum).unwrap();
    manager.install(&artifact, true).unwrap();

    let installed = manager.list().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].target, "yandexplum");
    assert_eq!(
        manager.which("share/example.txt").unwrap(),
        Some("example/alias".into())
    );
}
