# YAM v2 agent guide

YAM v2 is a package manager for Yandex Stations. Its core responsibility is
to validate, resolve, install, link, update and remove packages. Service
supervision is planned, not implemented (see "Implementation status").
It deliberately does **not** know about Lua, Quasar IPC, a web panel, or a
specific package host. Those are independent packages and hosts.

## Implementation status

Rules below describe the intended design. Items marked **(planned)** are not
implemented in `src/` yet; do not document or describe them as working, and do
not add them unless asked.

Implemented: `.yampkg` pack/validate/install/remove, SQLite state, exclusive
`$YAM_RUN/yam.lock`, rollback of a failed transaction, `conflicts`/`replaces`
handling on install, registries (`registry`, `search`, `info`, `plan`),
signed self-`update`, `doctor`, `repair`, `which`, `files`, Entware detection
and listing, service declarations (stored and shown by `service list`),
PC-side publication rules (`yam-publish check|pack|release`).

Planned (not implemented): package hooks (`preinstall`, `postinstall`,
`upgrade`), triggers, the service supervisor, `YAM_*` hook/service
environment, `yam autoremove`, a `yam rollback` command, `--no-recommends`,
installing `recommends` from a registry, config `.new` handling,
`yam convert-yap`, the interactive install/remove confirmation and plan display,
stopping dependent services on removal, config-hash and firmware tracking in
SQLite, OTA firmware comparison at boot.

## Scope and migration

- v2 code lives on `main`. The old v1 code (`.yap`/`plugin.toml`, `yamd`) is
  kept only on the `legacy` branch; do not port it back into `main`.
- v2 uses the incompatible `.yampkg` format. Do not revive `.yap` or
  `plugin.toml` in `main`.
- A v1-to-v2 converter, `yam convert-yap <archive.yap>`, is **planned** and not
  implemented. When added, it must produce a reviewable v2 source tree and
  must not install, execute hooks, or infer privileged behaviour.
- Lua, Quasar, the web UI and `.yap` plugins live in the separate Luna project
  (https://github.com/nyakokitsu/luna), package `yam/luna`.
- Never create a release or move a version tag unless the user explicitly
  requests it.

## Repository map

- `src/bin/yam.rs` — user-facing CLI (clap). Global `--platform`, `--root`.
  Commands: `install`, `remove`, `list`, `files`, `which`, `doctor`,
  `repair`, `update`, `entware status|list`, `service list`,
  `registry list|add|remove`, `search`, `info`, `plan`.
- `src/bin/yam_publish.rs` — PC publisher: `check`, `pack`, `publish`,
  `release`.
- `src/bin/yam_sign_update.rs` — update signing tool.
- `src/lib.rs` — library root.
- `src/publish/` — PC-side publishing: `rules.rs` (local publication rules,
  codes `PKG00N`), later credentials and the registry API client.
- `src/v2/` — package manager: `mod.rs`, `core.rs` (includes `manager.rs` via
  `include!`), `core_tests.rs`, `package.rs`, `registry.rs`, `update.rs`,
  `version.rs`.
- `platforms/` — platform profiles; add one YAML file per supported target.
- `docs/README.md` (catalogue), `docs/CLI.md`, `docs/PACKAGING.md`,
  `docs/PLATFORMS.md`, `docs/REGISTRY.md`, `docs/PUBLISHING-RULES.md` — public
  documentation.
- `install.sh` / `install.ps1` — deployment scripts. They write the Micro boot
  hook `/etc/init.d/S99yam`; `--remove-old` also removes the legacy hook and
  `/data/quazar/yam`. `--uninstall`/`-Uninstall` removes `$YAM_ROOT` only when
  it holds the installer-written `bin/yam-run` wrapper naming that root, plus
  the Micro link/hook that point at it; success is reported solely by the
  device printing `YAM-UNINSTALL-DONE`, because the stations' `adb shell` does
  not propagate exit codes. `install.ps1` ships multi-line device scripts
  base64-encoded (`… | base64 -d | sh`): Windows PowerShell 5.1 does not
  escape embedded double quotes in native arguments. Every read-write remount
  of `/` must restore read-only via a trap.
- `.github/workflows/ci.yml`, `release.yml` — CI and release.

Do not place package payloads, target binaries, registry downloads, device
dumps, logs, generated indexes, secrets, or credentials in the repository.

## Required checks

After changing Rust or shell code, run these. `ci.yml` runs `fmt`, `test` and
`check`; `release.yml` runs `fmt`, `test` and `sh -n install.sh`.

```sh
cargo fmt --check
cargo test --locked
cargo check --locked
sh -n install.sh
```

After changing PowerShell, run a parser check when PowerShell is available:

```powershell
[void][scriptblock]::Create((Get-Content -Raw ./install.ps1))
```

For code shipped to the aarch64 profiles (all except `mango` and
`jbl_link_music`), build the target as CI and release do:

```sh
docker run --rm -v "$PWD":/home/rust/src -w /home/rust/src \
  messense/rust-musl-cross:aarch64-musl cargo build --release --locked --bin yam
```

Run the narrowest relevant package-format, resolver and transaction tests too.
Before committing, require a clean `git diff --check`. Never commit `target/`.

## Core package-manager rules

### v2 source and artifact format

- Source metadata is `yam.yaml`; package artifacts are `.yampkg` files.
- A `.yampkg` is a `tar.zst` archive containing root `MANIFEST.yaml`, `files/`
  and optional `SOURCE`. Do not revive ZIP, `.yap`, or `plugin.toml` for v2.
- `yam-publish pack --platform <platform>` generates the artifact manifest from source
  metadata. It expands only `$YAM_TARGET` and `$YAM_TRIPLE` in `files` paths.
- A generated manifest contains exactly one target and hashes every payload
  file. It does not retain the source `files` mapping. Provenance is not
  recorded yet **(planned)**; `pack` never writes `SOURCE`, extraction only
  accepts it.
- Package names are `<namespace>/<name>`. Validate both components strictly;
  do not reuse the legacy unrestricted package ID parser.
- Versions are SemVer. Pre-releases are never selected implicitly.
- Target `any` is only for target-independent payloads. Native code must name
  one or more concrete platform profiles. YAM cannot verify this; it is an
  author rule.

### Publication rules

- Published combinations of `(name, version, target)` are immutable. The
  registry enforces this; the client must report a conflict and tell the author
  to bump SemVer, never retry the same upload.
- `src/publish/rules.rs` is the client-side mirror of the publication
  contract. Rule codes `PKG001`–`PKG011` are stable and documented in
  `docs/PUBLISHING-RULES.md`; do not renumber them.
- Client rules are convenience, not a trust boundary. Every rule they check
  must also be enforced by the registry when it accepts an upload.
- Local rules must not drift from `Manager::pack`/`validate_manifest`:
  rejecting something `pack` accepts is a bug unless the stricter rule is
  documented and intentional.
- `pack` resolves payload sources through `canonicalize` and refuses a path
  that escapes the package source directory.
- Do not add secret detection that logs the matching value; report only the
  file path and the kind of secret.

### Archive safety

Currently: entries are validated while streaming into staging (no separate
pre-pass); payload entries must be regular files or directories, root
`MANIFEST.yaml` (first) and optional `SOURCE` are allowed; absolute paths and
`..`/root/prefix components are rejected; duplicate path strings are rejected,
but there is no canonicalization/containment check **(planned)**. The
compressed size is checked before opening, per-file and total unpacked sizes
while streaming. Registry installs verify the signed index and artifact
size/SHA-256 before extraction; a local `.yampkg` has no signature check.
`files_sha256` is compared to the extracted payload before linking.

- Validate archives before extraction and validate again while extracting.
- Permit only regular files and directories in `files/`; reject symlinks,
  hardlinks, devices, FIFOs, absolute paths, `..`, duplicate normalized paths,
  and paths that escape the destination after canonicalization.
- Enforce compressed and unpacked size limits before allocating unbounded disk
  space. Extraction happens in a fresh staging directory under YAM's root.
- Verify registry hash/signature before extraction and verify `files_sha256`
  before a package becomes active.
- Never execute a hook, service, binary, or parser supplied by an unverified
  archive.

### Filesystem ownership and layout

- Logical package data lives below the profile's `$YAM_ROOT`, not hard-coded
  `/data/quazar`, `/opt`, or `/system` paths.
- Use the v2 layout: `pkgs/`, `bin/`, `share/`, `etc/`, `var/`, `db/` and a
  profile-defined runtime directory. Currently `Manager::open` creates `pkgs/`,
  `bin/`, `share/`, `db/` and `.staging/` under the root; `etc`/`var` are
  profile paths YAM does not create; the run directory is created on locking.
- Package payloads are version directories
  `pkgs/<namespace>/<name>/<version>/`, never modified in place. `current` is
  the only mutable version selector. Upgrade and removal delete the old
  version directory. Currently common-tree links are absolute and point at the
  version directory (`…/pkgs/example/hello/1.0.0/bin/hello`), not through
  `current`, so an upgrade has to replace every link.
- Only YAM creates links in the common tree. Archive-provided links are banned.
- `bin/` and `share/` links are file-by-file; common-tree directories are real
  directories. Do not link an entire directory into another package's tree.
- A common-tree path has exactly one recorded owner. Refuse collisions unless
  the transaction explicitly resolves them through `replaces`.
- `lib/` remains private: only `bin/` and `share/` are linked. Exporting a
  library is **(planned)** and must be an explicit, validated feature.
- For non-`yam/` namespaces, require a namespace-prefixed component in every
  linked `share/` path, such as `nikita.soloist`.

### SQLite state

- Implemented: SQLite (`db/state.sqlite3`) records installed packages and
  versions, payload paths, file hashes, links and path ownership, `provides`,
  dependencies and service declarations. An interrupted transaction is marked
  by the file `db/transaction.json`, not a table.
- **(planned)** config hashes and firmware/profile observations; no table
  exists for them yet.
- Use database transactions for all metadata mutations. Keep filesystem changes
  in an explicit, reversible transaction plan; do not rely on a best-effort
  sequence of ad-hoc writes.
- Hold an exclusive `flock` on `$YAM_RUN/yam.lock` for the complete package
  transaction (`install`, `remove`, `repair`; non-blocking, fails if busy).
  Hosts may take a shared lock while scanning shared directories; YAM itself
  never takes a shared lock.
- Record enough information to implement `yam doctor`, `yam repair`,
  `yam which`, `yam files` and safe removal without guessing. A
  `yam rollback` command is **(planned)**.
- Do not put mutable runtime state, logs, cache downloads or lock files into
  SQLite when a normal file is a better fit.

### Dependency resolution

- Resolve before downloading or changing disk state.
- Support `depends`, `recommends`, `enhances`, `provides`, `conflicts` and
  `replaces` exactly as documented. `depends` is mandatory.
  Currently: `depends`, `provides`, `conflicts` and `replaces` are enforced;
  `recommends` is stored but registry install does not install it;
  `enhances` is parsed into the manifest and otherwise unused;
  `--no-recommends` is **(planned)**.
- Parse and test all version constraints: `=`, `>=`, `<`, `^`, `~` and compound
  (space-separated) constraints. Do not compare versions lexicographically.
  `version.rs` rejects a word not starting with one of these, e.g. `>1.0`
  or `*`; `<=` passes because it starts with `<`.
- A short virtual dependency resolves to an already installed provider first;
  otherwise automatically select only an official `yam/` provider. Require an
  explicit user selection for another namespace. Implemented in
  `registry.rs::resolve_short`; multiple `yam/` providers are an error.
- Detect dependency cycles, incompatible constraints, multiple providers and
  path ownership conflicts before modifying the filesystem. Currently cycles,
  constraints and providers are checked for the whole registry graph before
  download; `depends`/`conflicts`/`replaces` and path ownership are checked per
  package inside `install` before its disk phase. A registry graph installs
  package by package and is not atomic: on a later failure `yam.rs` makes a
  best-effort `remove` of packages that were newly installed in this run
  (errors ignored); packages upgraded earlier in the graph are not restored.
- **(planned)** `yam autoremove` will remove only orphaned packages installed solely as
  dependencies. Never auto-remove an explicitly requested package.

### Install, upgrade and removal transactions

Steps 2, 3 and 6–8 are partly **(planned)**: there is no metadata/permission
display or interactive prompt yet, and hooks, triggers and service restarts do
not exist. The order also differs today: `install` takes the lock first, then
extracts and validates the artifact, refuses `replaces` removals and Entware
requests without `--yes`, runs `opkg install`, links payloads, switches
`current` and commits metadata. Only after that commit are replaced packages
removed, each in its own DB transaction (`manager.rs:287-295`): a failure
there leaves the new package plus some replaced ones installed. Entware
packages installed before a failed payload step are not removed either.
Registry artifacts are downloaded and hash-checked before all of this.

1. Resolve dependencies, targets, conflicts, replaces and Entware requests.
2. Display package metadata, declared permissions and planned changes.
3. Require confirmation unless `--yes` was passed.
4. Download, authenticate and validate every artifact.
5. Take the exclusive lock and build the complete link/ownership plan.
6. Run `preinstall`, stage payloads, switch `current`, and update links.
7. Run `postinstall` or `upgrade`, then commit metadata.
8. Release the lock, execute affected triggers once, then restart services.

- On failure before commit, restore the previous `current` link, common-tree
  links and database state. Preserve useful diagnostics.
- **(planned)** Hooks run as root with a timeout and the documented `YAM_*` environment.
  They must be idempotent. Hook failure rolls back the transaction except for
  post-transaction trigger failures, which are warnings.
- **(planned)** Stop dependent services before removing their provider. Present the complete
  reverse dependency plan and require confirmation. Currently `remove` refuses
  a package that installed packages still depend on.
- **(planned)** Preserve user-modified config. On upgrade, place a changed new default beside
  it as `.new`; compare against the recorded previous default hash.

### Services, triggers and platform profiles

Currently services are only validated declarations stored in the database and
shown by `yam service list`; nothing starts or supervises them.

- Services use the neutral manifest schema: `exec`, `restart`, `after`, `env`
  and `oneshot`. Do not accept systemd, OpenRC or BusyBox init scripts inside
  packages. Currently unenforced: `deny_unknown_fields` only rejects unknown
  YAML keys; payload files such as `etc/init.d/...` are not inspected.
- A platform profile selects the init adapter and durable boot hook. Never
  hard-code a model's sysfs path, ABI, writable partition, init script path or
  firmware-version path in generic package-manager code. Currently `init` and
  `boot_hook` are parsed but unused in `src/`; the installers write the Micro
  boot hook themselves.
- **(planned)** The default supervisor owns service processes, logs and restart policy.
  Ensure it kills an entire process group, prevents duplicate starts, and
  avoids restart loops.
- **(planned)** All service and hook environments include `YAM_ROOT`, `YAM_PKG`, `YAM_ETC`,
  `YAM_VAR`, `YAM_RUN` and `YAM_PLATFORM`; add `/opt/bin:/opt/sbin` only when
  Entware is active.
- **(planned)** Triggers are glob/path based, run once per committed transaction, receive
  changed paths on stdin, and run only after the package lock is released.
- **(planned)** OTA handling compares the profile-defined firmware version at boot
  (`firmware_version_file` is parsed but unused). `repair` restores only
  recorded YAM links, refuses non-link obstructions and foreign symlinks,
  clears stale `.staging/` and the `db/transaction.json` marker.
- `system_overrides`, bind mounts, `LD_PRELOAD` and overlayfs are out of scope
  for v2's first implementation. There is no code path for them; such keys are
  rejected only as unknown fields by `deny_unknown_fields`.

### Entware

- Entware is opt-in. A package may declare `entware.packages`, but installation
  must show those external packages and ask for confirmation unless `--yes` is
  supplied. Currently there is no prompt: install fails without `--yes`, and
  with it runs `opkg install` before the payload is linked into the tree.
- Detect Entware through the platform adapter, normally `/opt/bin/opkg`; never
  assume `/opt` exists on every target.
- Do not model Entware-owned files as YAM package files and never let a YAM
  transaction remove an Entware package implicitly.
- Provide transparent diagnostics through `yam entware ...` and `yam doctor`.
  Implemented: `yam entware status|list`. **(planned)** `yam doctor` does not
  check Entware yet, so a missing Entware dependency is not reported as
  unhealthy.

## Lua runtime rules

Luna is a **host package**, published as `yam/luna`; it is not a special case
in the YAM package manager.

- Luna discovers plugin files under `$YAM_ROOT/share/luna/plugins/`. YAM only
  installs and links files;
  it does not parse, start or authorize Lua plugins.
- Lua plugin packages are ordinary `.yampkg` artifacts. Keep runtime-specific
  manifest, permission and API metadata inside the plugin's own files.
- Do not add Lua permissions, Quasar IPC permissions, `alice.*` APIs or LED
  assumptions to `MANIFEST.yaml` or the generic resolver.
- The runtime owns plugin isolation, cancellation, compatibility checks and
  runtime API authorization. It should use a shared lock while scanning plugin
  directories and react to its own reload trigger.
- Hardware access is deny-by-default. A new Lua API requires a runtime
  permission, validation of its manifest declaration, documentation and a
  hardware capability check.
- Do not expose raw Quasar or system control merely because a package declares
  a generic v2 permission. Keep powerful operations explicitly scoped in the
  runtime's own format.
- Long-running Lua code must be cancellable. Do not block the runtime executor
  indefinitely; provide cancellation-aware sleep, I/O and event waiting.
- Runtime code must discover hardware via platform capabilities or validated
  adapters. Never infer unknown-device paths from a model name.
- A Luna package can declare package-level dependencies like any other host.
  Lua plugins should depend on the Luna package/version they require.

## General Rust and CLI rules

- Prefer small modules with explicit ownership boundaries over growing
  `src/v2/core.rs` and `src/bin/yam.rs` with unrelated logic.
- Return actionable errors: package name/version, target, failing path and the
  failed validation rule. Do not hide a partial transaction behind `Ok(())`.
- Paths received from archives, manifests, CLI arguments and registries are
  untrusted. Validate them before creating paths or invoking a process.
- Never build shell command strings from package-controlled values. Use process
  argument arrays and a deliberately constructed environment.
- Treat registry metadata, package archives, hooks and service definitions as
  untrusted until verified. Never log tokens, authorization headers, private
  config or raw audio/speech data.
- Preserve user work: do not reset, checkout, delete or overwrite unrelated
  changes. Stage only the files required by the current task.
- Update the matching public documentation (`docs/`) whenever a manifest, CLI
  or package lifecycle contract changes.
