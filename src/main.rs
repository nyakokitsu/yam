use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{delete, get, post},
    Json, Router,
};
use clap::Parser;
use flate2::read::GzDecoder;
use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Cursor, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Mutex, task};
use tower_http::trace::TraceLayer;
use url::Url;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8099")]
    listen: String,
    #[arg(long, default_value = "/opt/yam")]
    data_dir: PathBuf,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    led: LedSettings,
    #[serde(default)]
    web: WebSettings,
    #[serde(default)]
    allow_plugin_binaries: bool,
    #[serde(default)]
    updates: UpdateSettings,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateSettings {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_update_interval")]
    interval_minutes: u64,
}
fn default_update_interval() -> u64 {
    360
}
impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: default_update_interval(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WebSettings {
    #[serde(default = "default_true")]
    ui: bool,
    #[serde(default = "default_true")]
    api: bool,
}
fn default_true() -> bool {
    true
}
impl Default for WebSettings {
    fn default() -> Self {
        Self {
            ui: true,
            api: true,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedSettings {
    profile: String,
    #[serde(default = "default_led_count")]
    count: usize,
    #[serde(default = "default_led_root")]
    sysfs_root: String,
    #[serde(default)]
    path_template: Option<String>,
}
fn default_led_count() -> usize {
    6
}
fn default_led_root() -> String {
    "/sys/class/leds".into()
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            // Unknown hardware gets the `custom` template; `main` swaps in a detected profile.
            led: LedSettings {
                profile: "custom".into(),
                count: 6,
                sysfs_root: default_led_root(),
                path_template: None,
            },
            web: WebSettings::default(),
            allow_plugin_binaries: false,
            updates: UpdateSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    id: String,
    name: String,
    version: String,
    entry: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    settings: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    network: NetworkPolicy,
    #[serde(default)]
    input: InputPolicy,
    #[serde(default)]
    ipc: IpcPolicy,
    #[serde(default)]
    commands: CommandPolicy,
    #[serde(default)]
    compatibility: Compatibility,
    #[serde(default)]
    launch: Launch,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct NetworkPolicy {
    /// Exact host names or a DNS suffix prefixed with `*.`.
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    allow_insecure_http: bool,
}
/// Input devices a package may read through `alice.input.next_event`, by kernel device name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InputPolicy {
    #[serde(default)]
    devices: Vec<String>,
}
/// Quasar services a package may access through the local IPC bridge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IpcPolicy {
    #[serde(default)]
    services: Vec<String>,
}
/// Voice commands a package may receive from aliced.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommandPolicy {
    #[serde(default)]
    phrases: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Launch {
    #[serde(default)]
    kind: LaunchKind,
    #[serde(default)]
    mode: LaunchMode,
    #[serde(default)]
    path: String,
    #[serde(default)]
    args: Vec<String>,
    /// Runs `path` through this interpreter; only `sh` is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interpreter: Option<String>,
    /// Package-relative directories prepended to `LD_LIBRARY_PATH`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    library_paths: Vec<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LaunchKind {
    #[default]
    Lua,
    Binary,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LaunchMode {
    #[default]
    Oneshot,
    Service,
}
impl Launch {
    fn path<'a>(&'a self, manifest: &'a Manifest) -> &'a str {
        if self.path.is_empty() {
            &manifest.entry
        } else {
            &self.path
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Compatibility {
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    features: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceProfile {
    model: String,
    architecture: String,
    features: Vec<String>,
    source: String,
}
#[derive(Debug, Clone)]
struct Sensor {
    id: String,
    kind: String,
    path: PathBuf,
    unit: String,
}
#[derive(Debug, Serialize)]
struct SensorInfo {
    id: String,
    kind: String,
    unit: String,
    value: Option<f64>,
}
#[derive(Debug, Serialize)]
struct ClockInfo {
    available: bool,
    segments: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct ClockRequest {
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    brightness: Option<u8>,
    #[serde(default)]
    segments: Option<HashMap<String, u8>>,
    #[serde(default)]
    off: bool,
}
#[derive(Debug, Serialize)]
struct VolumeInfo {
    percent: Option<u8>,
    raw: String,
}
#[derive(Debug, Deserialize)]
struct VolumeRequest {
    percent: u8,
}
#[derive(Debug, Serialize)]
struct SystemInfo {
    hostname: String,
    uptime: String,
    meminfo: String,
}
#[derive(Debug, Deserialize)]
struct BluetoothRequest {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    mac: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
struct PluginInfo {
    manifest: Manifest,
    /// Absolute package directory on the speaker, used by `yam tui`.
    directory: PathBuf,
    enabled: bool,
    service_enabled: bool,
    running: bool,
    compatible: bool,
    incompatibility: Option<String>,
}
#[derive(Clone)]
struct App {
    root: PathBuf,
    settings: Arc<Mutex<Settings>>,
    device: Arc<Mutex<DeviceProfile>>,
    led: Arc<Led>,
    plugins: Arc<Mutex<PluginManager>>,
    runtime_tokens: Arc<Mutex<HashMap<String, String>>>,
    runtime_api_url: String,
}
#[derive(Clone)]
struct Led {
    settings: Arc<Mutex<LedSettings>>,
}
#[derive(Deserialize)]
struct Frame {
    colors: Vec<String>,
    #[serde(default = "frame_delay")]
    delay_ms: u64,
}
#[derive(Deserialize)]
struct Invocation {
    #[serde(default)]
    args: Vec<String>,
}
fn frame_delay() -> u64 {
    100
}

impl Led {
    async fn draw(&self, colors: Vec<String>) -> Result<()> {
        let c = self.settings.lock().await.clone();
        if colors.len() != c.count {
            bail!("expected {} colors, got {}", c.count, colors.len())
        }
        for (i, hex) in colors.iter().enumerate() {
            let (r, g, b) = parse_color(hex)?;
            self.write(&c, "red", i, r).await?;
            self.write(&c, "green", i, g).await?;
            self.write(&c, "blue", i, b).await?;
        }
        Ok(())
    }
    async fn off(&self) -> Result<()> {
        let c = self.settings.lock().await.clone();
        if c.profile == "plum" {
            return self.release_plum(&c);
        }
        if c.profile == "midi" {
            return self.release_midi(&c);
        }
        self.draw(vec!["#000000".into(); c.count]).await
    }
    async fn write(&self, c: &LedSettings, channel: &str, i: usize, value: u8) -> Result<()> {
        let path = led_brightness_path(c, channel, i)?;
        fs::write(&path, value.to_string()).with_context(|| format!("write {}", path.display()))
    }
    // On Lite 2, manual RGB brightness detaches platformd's framebuffer trigger.
    // Release only the known three RGB LEDs; never clear a new Alice frame or
    // touch the white clock segments. Validate every trigger before any write.
    fn release_plum(&self, c: &LedSettings) -> Result<()> {
        if c.profile != "plum" || c.count != 3 {
            bail!("Lite 2 release requires the known three-LED plum profile")
        }
        let mut triggers = Vec::new();
        for i in 0..c.count {
            for channel in ["red", "green", "blue"] {
                let trigger = led_brightness_path(c, channel, i)?.with_file_name("trigger");
                let available = fs::read_to_string(&trigger)
                    .with_context(|| format!("read trigger {}", trigger.display()))?;
                if !available
                    .split_whitespace()
                    .any(|s| s.trim_start_matches('[').trim_end_matches(']') == "led_ring_fb")
                {
                    bail!(
                        "native led_ring_fb trigger unavailable at {}",
                        trigger.display()
                    )
                }
                triggers.push(trigger);
            }
        }
        for trigger in triggers {
            fs::write(&trigger, "led_ring_fb")
                .with_context(|| format!("restore trigger {}", trigger.display()))?;
        }
        Ok(())
    }
    // Writing brightness detaches the `ledfb-ring` trigger used by platformd.
    fn release_midi(&self, c: &LedSettings) -> Result<()> {
        for i in 0..c.count {
            for channel in ["red", "green", "blue"] {
                let brightness = led_brightness_path(c, channel, i)?;
                fs::write(&brightness, "0")
                    .with_context(|| format!("write {}", brightness.display()))?;
                let trigger = brightness.with_file_name("trigger");
                fs::write(&trigger, "ledfb-ring")
                    .with_context(|| format!("restore trigger {}", trigger.display()))?;
            }
        }
        Ok(())
    }
    async fn play_led(&self, path: PathBuf, cancelled: &AtomicBool) -> Result<()> {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("read pattern {}", path.display()))?;
        let (looped, frames) = parse_pattern(&source)?;
        if frames.is_empty() {
            bail!("pattern has no frames")
        };
        loop {
            for f in &frames {
                self.draw(f.colors.clone()).await?;
                let mut left = f.delay_ms;
                while left > 0 {
                    if cancelled.load(Ordering::Relaxed) {
                        bail!("plugin stopped")
                    }
                    let slice = left.min(20);
                    tokio::time::sleep(Duration::from_millis(slice)).await;
                    left -= slice;
                }
            }
            if !looped {
                break;
            }
        }
        Ok(())
    }
}
/// Known sysfs ring layouts: (profile, LED name prefix, first index, LED count).
const LED_PROFILES: [(&str, &str, usize, usize); 4] = [
    ("micro", "micro:", 1, 3),
    ("midi", "cucumber:", 0, 24),
    ("plum", "plum:", 1, 3),
    ("mini2", "mini2:", 1, 6),
];
/// `index` is zero-based; profiles number their sysfs LEDs from their own base.
fn led_brightness_path(c: &LedSettings, channel: &str, index: usize) -> Result<PathBuf> {
    let root = PathBuf::from(&c.sysfs_root);
    if let Some((_, prefix, base, _)) = LED_PROFILES.iter().find(|p| p.0 == c.profile) {
        return Ok(root.join(format!("{prefix}{channel}:ring{}/brightness", index + base)));
    }
    match c.profile.as_str() {
        "sysfs-ring" => Ok(root.join(format!("{channel}:ring{}/brightness", index + 1))),
        "custom" => Ok(PathBuf::from(
            c.path_template
                .as_ref()
                .context("custom profile needs path_template")?
                .replace("{root}", &c.sysfs_root)
                .replace("{channel}", channel)
                .replace("{index}", &(index + 1).to_string()),
        )),
        p => bail!("unknown LED profile {p}"),
    }
}
/// Picks the first known ring layout present in sysfs.
fn detect_led_profile(sysfs_root: &str) -> Option<(&'static str, usize)> {
    LED_PROFILES
        .iter()
        .find(|(_, prefix, base, _)| {
            Path::new(sysfs_root)
                .join(format!("{prefix}red:ring{base}/brightness"))
                .exists()
        })
        .map(|(profile, _, _, count)| (*profile, *count))
}
fn parse_color(s: &str) -> Result<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid RGB color {s}")
    };
    Ok((
        u8::from_str_radix(&h[0..2], 16)?,
        u8::from_str_radix(&h[2..4], 16)?,
        u8::from_str_radix(&h[4..6], 16)?,
    ))
}
fn parse_pattern(s: &str) -> Result<(bool, Vec<Frame>)> {
    let mut looped = false;
    let mut frames = vec![];
    for line in s
        .lines()
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.starts_with('#'))
    {
        if line.eq_ignore_ascii_case("loop") {
            looped = true;
            continue;
        }
        if line.eq_ignore_ascii_case("gammacorrection") {
            continue;
        };
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() < 2 {
            continue;
        };
        let delay = p.last().unwrap().parse().context("bad frame delay")?;
        frames.push(Frame {
            colors: p[..p.len() - 1].iter().map(|x| format!("#{x}")).collect(),
            delay_ms: delay,
        });
    }
    Ok((looped, frames))
}

struct PluginManager {
    root: PathBuf,
    enabled: HashSet<String>,
    service_enabled: HashSet<String>,
    running: HashMap<String, Arc<AtomicBool>>,
}
impl PluginManager {
    fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let enabled = match fs::read_to_string(root.join("enabled.json")) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "enabled.json is corrupt; all packages start disabled");
                HashSet::new()
            }),
            Err(_) => HashSet::new(),
        };
        let service_path = root.join("services.json");
        let service_enabled = match fs::read_to_string(&service_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, "services.json is corrupt; no packages auto-start");
                HashSet::new()
            }),
            Err(_) => {
                // Migrate old mode-based services exactly once when no explicit
                // service state exists yet.
                let legacy = fs::read_dir(&root)?
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| {
                        let manifest = fs::read_to_string(entry.path().join("plugin.toml")).ok()?;
                        let manifest = toml::from_str::<Manifest>(&manifest).ok()?;
                        (enabled.contains(&manifest.id)
                            && manifest.launch.mode == LaunchMode::Service)
                            .then_some(manifest.id)
                    })
                    .collect::<HashSet<_>>();
                if !legacy.is_empty() {
                    write_atomic(&service_path, &serde_json::to_vec_pretty(&legacy)?)?;
                }
                legacy
            }
        };
        Ok(Self {
            root,
            enabled,
            service_enabled,
            running: HashMap::new(),
        })
    }
    fn save(&self) -> Result<()> {
        write_atomic(
            &self.root.join("enabled.json"),
            &serde_json::to_vec_pretty(&self.enabled)?,
        )?;
        write_atomic(
            &self.root.join("services.json"),
            &serde_json::to_vec_pretty(&self.service_enabled)?,
        )
    }
    fn list(&self, device: &DeviceProfile) -> Result<Vec<PluginInfo>> {
        let mut out = vec![];
        for e in fs::read_dir(&self.root)? {
            let e = e?;
            if !e.file_type()?.is_dir() {
                continue;
            };
            let p = e.path().join("plugin.toml");
            if let Ok(x) = fs::read_to_string(p) {
                if let Ok(manifest) = toml::from_str::<Manifest>(&x) {
                    let reason = compatibility_error(&manifest, device);
                    out.push(PluginInfo {
                        directory: e.path(),
                        enabled: self.enabled.contains(&manifest.id),
                        service_enabled: self.service_enabled.contains(&manifest.id),
                        running: self.running.contains_key(&manifest.id),
                        compatible: reason.is_none(),
                        incompatibility: reason,
                        manifest,
                    })
                }
            }
        }
        Ok(out)
    }
    fn manifest(&self, id: &str) -> Result<Manifest> {
        validate_id(id)?;
        Ok(toml::from_str(&fs::read_to_string(
            self.root.join(id).join("plugin.toml"),
        )?)?)
    }
    fn install(&mut self, bytes: &[u8]) -> Result<Manifest> {
        let stage = self.root.join(format!(".install-{}", Uuid::new_v4()));
        let budget = unpack_budget(&self.root)?;
        fs::create_dir(&stage)?;
        let result = self.install_staged(bytes, &stage, budget);
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }
    fn install_staged(&self, bytes: &[u8], stage: &Path, budget: u64) -> Result<Manifest> {
        if bytes.starts_with(b"PK\x03\x04") {
            extract_zip_yap(bytes, stage, budget)?
        } else if bytes.starts_with(&[0x1f, 0x8b]) {
            extract_tar_gz_yap(bytes, stage, budget)?
        } else {
            bail!(".yap must be a ZIP or gzip-compressed TAR archive")
        }
        let manifest_text = fs::read_to_string(stage.join("plugin.toml"))
            .context(".yap must contain root plugin.toml")?;
        let m: Manifest = toml::from_str(&manifest_text)?;
        validate_id(&m.id)?;
        safe_relative(&m.entry)?;
        safe_relative(m.launch.path(&m))?;
        for directory in &m.launch.library_paths {
            safe_relative(directory)?;
        }
        if m.launch.interpreter.as_deref().is_some_and(|x| x != "sh") {
            bail!("launch.interpreter supports only \"sh\"")
        }
        if m.launch.kind == LaunchKind::Binary && !m.permissions.iter().any(|p| p == "exec") {
            bail!("binary packages require the exec permission")
        }
        validate_input_policy(&m)?;
        validate_ipc_policy(&m)?;
        validate_command_policy(&m)?;
        if m.launch.kind == LaunchKind::Binary {
            let executable = stage.join(m.launch.path(&m));
            if !executable.is_file() {
                bail!("binary launch path does not name a file")
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for entry in walkdir::WalkDir::new(stage) {
                    let entry = entry?;
                    if entry.file_type().is_file() {
                        let mut permissions = fs::metadata(entry.path())?.permissions();
                        permissions.set_mode(permissions.mode() | 0o111);
                        fs::set_permissions(entry.path(), permissions)?;
                    }
                }
            }
        }
        let target = self.root.join(&m.id);
        if target.exists() {
            fs::remove_dir_all(&target)?
        };
        fs::rename(stage, target)?;
        Ok(m)
    }
    fn remove(&mut self, id: &str) -> Result<()> {
        validate_id(id)?;
        let p = self.root.join(id);
        if !p.exists() {
            bail!("plugin not found")
        };
        if let Some(token) = self.running.remove(id) {
            token.store(true, Ordering::Relaxed);
        }
        fs::remove_dir_all(p)?;
        self.enabled.remove(id);
        self.service_enabled.remove(id);
        self.save()
    }
}
fn archive_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe archive path")
    }
    Ok(path.to_path_buf())
}
fn set_archive_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    }
    Ok(())
}
const MAX_UNPACKED_BYTES: u64 = 32 * 1024 * 1024;
const FREE_SPACE_RESERVE: u64 = 8 * 1024 * 1024;
fn unpack_budget(directory: &Path) -> Result<u64> {
    Ok(MAX_UNPACKED_BYTES.min(available_bytes(directory)?.saturating_sub(FREE_SPACE_RESERVE)))
}
#[cfg(unix)]
fn available_bytes(directory: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", directory.display()));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}
#[cfg(not(unix))]
fn available_bytes(_: &Path) -> Result<u64> {
    Ok(u64::MAX)
}
fn copy_limited(source: impl Read, output: &mut fs::File, remaining: &mut u64) -> Result<()> {
    let written = std::io::copy(&mut source.take(*remaining + 1), output)?;
    if written > *remaining {
        bail!("package does not fit: unpacked size is limited to 32 MiB and 8 MiB of free space is kept")
    }
    *remaining -= written;
    Ok(())
}
fn extract_zip_yap(bytes: &[u8], stage: &Path, mut remaining: u64) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe archive path"))?
            .to_owned();
        let destination = stage.join(path);
        if entry.is_dir() {
            fs::create_dir_all(destination)?;
        } else {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mode = entry.unix_mode();
            let mut output = fs::File::create(&destination)?;
            copy_limited(&mut entry, &mut output, &mut remaining)?;
            if let Some(mode) = mode {
                set_archive_mode(&destination, mode)?;
            }
        }
    }
    Ok(())
}
fn extract_tar_gz_yap(bytes: &[u8], stage: &Path, mut remaining: u64) -> Result<()> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for item in archive.entries()? {
        let mut entry = item?;
        let path = archive_path(&entry.path()?)?;
        let destination = stage.join(path);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(destination)?;
        } else if entry_type.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mode = entry.header().mode()?;
            let mut output = fs::File::create(&destination)?;
            copy_limited(&mut entry, &mut output, &mut remaining)?;
            set_archive_mode(&destination, mode)?;
        } else {
            bail!("unsupported TAR archive entry")
        }
    }
    Ok(())
}
fn compatibility_error(m: &Manifest, d: &DeviceProfile) -> Option<String> {
    if !m.compatibility.models.is_empty() && !m.compatibility.models.iter().any(|x| x == &d.model) {
        return Some(format!(
            "requires model [{}], detected {}",
            m.compatibility.models.join(", "),
            d.model
        ));
    };
    let present: HashSet<_> = d.features.iter().collect();
    let missing: Vec<_> = m
        .compatibility
        .features
        .iter()
        .filter(|x| !present.contains(x))
        .cloned()
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!("missing feature(s): {}", missing.join(", ")))
    }
}
/// Converts model identifiers from device-tree, CLI and package manifests into
/// YAM's stable package-facing form (for example, `yandexmicro`).
fn recognized_model(value: &str) -> Option<&'static str> {
    let normalized = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    const MODELS: [(&str, &str); 7] = [
        ("yandexmicro", "yandexmicro"),
        ("yandexcucumber", "yandexmidi"),
        ("yandexplum", "yandexplum"),
        ("yandexmini2", "yandexmini_2"),
        ("stationmini2", "yandexmini_2"),
        ("yandexstation2", "yandexstation_2"),
        ("yandexmini", "yandexmini"),
    ];
    MODELS
        .iter()
        .find(|(needle, _)| normalized.contains(needle))
        .map(|(_, model)| *model)
}
fn canonical_model(value: &str) -> String {
    recognized_model(value)
        .map(str::to_owned)
        .unwrap_or_else(|| value.trim().to_ascii_lowercase())
}
fn detect_device(root: &Path, forced: Option<String>) -> DeviceProfile {
    if let Ok(raw) = fs::read_to_string(root.join("device.toml")) {
        if let Ok(mut p) = toml::from_str::<DeviceProfile>(&raw) {
            p.model = canonical_model(&p.model);
            return p;
        }
    }
    let arch = std::env::consts::ARCH.to_string();
    let text = [
        "/proc/device-tree/model",
        "/proc/device-tree/compatible",
        "/etc/os-release",
    ]
    .iter()
    .filter_map(|p| fs::read_to_string(p).ok())
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
    // Ring layout is the most reliable signal; device-tree strings cover the rest.
    // Mini 2 reports `yandex,station-mini2` in its device-tree compatible list.
    let model = forced.unwrap_or_else(|| {
        let by_leds = detect_led_profile(&default_led_root()).map(|(profile, _)| match profile {
            "micro" => "yandexmicro",
            "midi" => "yandexmidi",
            "plum" => "yandexplum",
            _ => "yandexmini_2",
        });
        by_leds
            .or_else(|| recognized_model(&text))
            .unwrap_or("unknown")
            .into()
    });
    let model = canonical_model(&model);
    // LED ring features depend on the configured profile and are added in `main`.
    let mut features = vec!["runtime.lua.v1".into(), "plugin.yap.v1".into()];
    if command_exists("amixer") {
        features.push("audio.volume.alsa".into())
    }
    if command_exists("aplay") {
        features.push("audio.play.aplay".into())
    }
    if command_exists("bluetoothctl") {
        features.extend([
            "bluetooth.bluez".into(),
            "bluetooth.scan".into(),
            "bluetooth.pair".into(),
        ])
    }
    if Path::new("/sys/class/backlight").exists() {
        features.push("display.backlight".into())
    }
    if Path::new("/sys/class/input").exists() {
        features.push("input.evdev".into())
    }
    let sensors = discover_sensors();
    if sensors.iter().any(|sensor| sensor.kind == "ambient_light") {
        features.push("sensor.ambient_light.iio".into())
    }
    if sensors.iter().any(|sensor| sensor.kind == "temperature") {
        features.push("sensor.temperature.sysfs".into())
    }
    if sensors.iter().any(|sensor| sensor.kind == "battery") {
        features.push("battery.sysfs".into())
    }
    if clock_available() {
        features.push("display.clock.segment.v1".into())
    }
    if arch.starts_with("arm") {
        features.push("arch.armhf".into())
    };
    if arch == "aarch64" {
        features.push("arch.aarch64".into())
    }
    DeviceProfile {
        model,
        architecture: arch,
        features,
        source: "auto (override: data-dir/device.toml or --model)".into(),
    }
}
/// Replaces `path` via a synced sibling file so a power cut never leaves it truncated.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut file =
        fs::File::create(&temporary).with_context(|| format!("write {}", temporary.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}
fn read_trimmed(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}
fn sorted_directories(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
fn discover_sensors() -> Vec<Sensor> {
    let mut sensors = Vec::new();
    for device in sorted_directories(Path::new("/sys/bus/iio/devices")) {
        let value = device.join("in_intensity_input");
        if value.is_file() {
            let name = device.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("ambient-light:{name}"),
                kind: "ambient_light".into(),
                path: value,
                unit: "lux".into(),
            });
        }
    }
    for zone in sorted_directories(Path::new("/sys/class/thermal")) {
        let value = zone.join("temp");
        if value.is_file() {
            let name = zone.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("temperature:{name}"),
                kind: "temperature".into(),
                path: value,
                unit: "celsius".into(),
            });
        }
    }
    for device in sorted_directories(Path::new("/sys/class/hwmon")) {
        let name = read_trimmed(&device.join("name")).unwrap_or_else(|_| {
            device
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });
        if let Ok(entries) = fs::read_dir(&device) {
            for entry in entries.flatten() {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if file_name.starts_with("temp") && file_name.ends_with("_input") {
                    sensors.push(Sensor {
                        id: format!("temperature:{name}:{file_name}"),
                        kind: "temperature".into(),
                        path: entry.path(),
                        unit: "celsius".into(),
                    });
                }
            }
        }
    }
    for supply in sorted_directories(Path::new("/sys/class/power_supply")) {
        let value = supply.join("capacity");
        if value.is_file() {
            let name = supply.file_name().unwrap_or_default().to_string_lossy();
            sensors.push(Sensor {
                id: format!("battery:{name}"),
                kind: "battery".into(),
                path: value,
                unit: "percent".into(),
            });
        }
    }
    sensors
}
fn sensor_value(sensor: &Sensor) -> Result<f64> {
    let mut value: f64 = read_trimmed(&sensor.path)?.parse()?;
    if sensor.kind == "temperature" && value.abs() > 1_000.0 {
        value /= 1_000.0;
    }
    Ok(value)
}
fn configured_light_sensor(name: &str) -> Result<PathBuf> {
    let sensor = discover_sensors()
        .into_iter()
        .find(|sensor| sensor.id == format!("ambient-light:{name}"))
        .context("ambient light sensor not found")?;
    Ok(sensor
        .path
        .parent()
        .context("invalid ambient light sensor path")?
        .to_path_buf())
}
enum ClockLayout {
    Mini2(String),
    Cucumber(String),
    /// Yandex Station Lite 2 (`plum`). Same `{prefix}clock{segment}` shape as
    /// Mini2, but positions are 0-based and the separator is three separate
    /// segments instead of PLUS/MINUS markers.
    Plum(String),
}
const CUCUMBER_EXTRA_SEGMENTS: [&str; 14] = [
    "H1", "H2", "L1", "L2", "M1", "M2", "P1", "P2", "P4", "P5", "Q1", "Q2", "Q4", "Q5",
];
impl ClockLayout {
    fn segments(&self) -> Vec<String> {
        // plum: four positions 0..3, eight parts A..H, and a three-segment
        // separator (DH/DM/DL) instead of PLUS1/PLUS2/MINUS. DEGREE is the
        // standalone marker next to the digits.
        if let ClockLayout::Plum(_) = self {
            let mut segments = Vec::new();
            for position in 0..=3 {
                for part in ["A", "B", "C", "D", "E", "F", "G", "H"] {
                    segments.push(format!("{part}{position}"));
                }
            }
            segments.extend(["DEGREE".into(), "DH".into(), "DM".into(), "DL".into()]);
            return segments;
        }
        let mut segments = Vec::new();
        for position in 1..=4 {
            for part in ["A", "B", "C", "D", "E", "F", "G"] {
                segments.push(format!("{part}{position}"));
            }
        }
        segments.extend(["PLUS1".into(), "PLUS2".into()]);
        match self {
            ClockLayout::Mini2(_) => segments.extend(["MINUS".into(), "DEGREE".into()]),
            ClockLayout::Cucumber(_) => {
                segments.push("DEGREE".into());
                segments.extend(CUCUMBER_EXTRA_SEGMENTS.iter().map(|x| x.to_string()));
            }
            ClockLayout::Plum(_) => unreachable!(),
        }
        segments
    }
    fn path(&self, segment: &str) -> PathBuf {
        match self {
            ClockLayout::Mini2(prefix) | ClockLayout::Plum(prefix) => {
                Path::new("/sys/class/leds").join(format!("{prefix}clock{segment}/brightness"))
            }
            ClockLayout::Cucumber(prefix) => {
                let name = match segment {
                    "PLUS1" => "p3".into(),
                    "PLUS2" => "q3".into(),
                    "DEGREE" => "deg".into(),
                    other => other.to_lowercase(),
                };
                Path::new("/sys/class/leds").join(format!("{prefix}white:clock-{name}/brightness"))
            }
        }
    }
    fn complete(&self) -> bool {
        self.segments()
            .iter()
            .all(|segment| self.path(segment).is_file())
    }
    // `sync:clock-N` = 1 freezes the Midi display until it is reset to 0.
    fn unlatch(&self) -> Result<()> {
        let ClockLayout::Cucumber(prefix) = self else {
            return Ok(());
        };
        let latch = format!("{prefix}sync:clock-");
        for entry in fs::read_dir("/sys/class/leds")?.flatten() {
            let brightness = entry.path().join("brightness");
            if entry.file_name().to_string_lossy().starts_with(&latch)
                && read_trimmed(&brightness)? != "0"
            {
                fs::write(brightness, "0")?;
            }
        }
        Ok(())
    }
}
// The LED set is fixed by the kernel driver, so the layout is probed once.
fn clock_layout() -> Option<&'static ClockLayout> {
    static LAYOUT: std::sync::OnceLock<Option<ClockLayout>> = std::sync::OnceLock::new();
    LAYOUT.get_or_init(probe_clock_layout).as_ref()
}
fn probe_clock_layout() -> Option<ClockLayout> {
    let names = fs::read_dir("/sys/class/leds")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    // plum is detected first: its names also end with "clockA1", so the Mini2
    // probe below would otherwise claim it and then fail `complete()`.
    let plum = names
        .iter()
        .filter_map(|name| name.strip_suffix("clockA0"))
        .map(|prefix| ClockLayout::Plum(prefix.into()));
    let upper = names
        .iter()
        .filter_map(|name| name.strip_suffix("clockA1"))
        .map(|prefix| ClockLayout::Mini2(prefix.into()));
    let cucumber = names
        .iter()
        .filter_map(|name| name.strip_suffix("white:clock-a1"))
        .map(|prefix| ClockLayout::Cucumber(prefix.into()));
    plum.chain(upper)
        .chain(cucumber)
        .find(ClockLayout::complete)
}
fn clock_segments() -> Vec<String> {
    clock_layout()
        .map(ClockLayout::segments)
        .unwrap_or_default()
}
fn clock_available() -> bool {
    clock_layout().is_some()
}
// platformd redraws the clock once a minute, so release restores its last frame instead of blanking.
struct ClockOwnership {
    system: Vec<(PathBuf, String)>,
    ours: Vec<(PathBuf, String)>,
}
static CLOCK_OWNERSHIP: std::sync::Mutex<Option<ClockOwnership>> = std::sync::Mutex::new(None);
fn clock_read(layout: &ClockLayout) -> Result<Vec<(PathBuf, String)>> {
    layout
        .segments()
        .iter()
        .map(|segment| {
            let path = layout.path(segment);
            let value = read_trimmed(&path)?;
            Ok((path, value))
        })
        .collect()
}
fn clock_write(segments: &HashMap<String, u8>) -> Result<()> {
    let layout = clock_layout().context("segment clock is not available")?;
    let mut ownership = CLOCK_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
    let current = clock_read(&layout)?;
    if ownership.as_ref().is_none_or(|o| o.ours != current) {
        *ownership = Some(ClockOwnership {
            system: current,
            ours: Vec::new(),
        });
    }
    layout.unlatch()?;
    for segment in layout.segments() {
        let value = segments.get(&segment).copied().unwrap_or(0);
        fs::write(layout.path(&segment), value.to_string())?;
    }
    if let Some(o) = ownership.as_mut() {
        o.ours = clock_read(&layout)?;
    }
    Ok(())
}
fn clock_release() -> Result<()> {
    let layout = clock_layout().context("segment clock is not available")?;
    let mut ownership = CLOCK_OWNERSHIP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(o) = ownership.take() else {
        return Ok(());
    };
    if clock_read(&layout)? != o.ours {
        return Ok(());
    }
    layout.unlatch()?;
    for (path, value) in o.system {
        fs::write(path, value)?;
    }
    Ok(())
}
fn validate_clock_segments(segments: &HashMap<String, u8>) -> Result<()> {
    let valid = clock_segments().into_iter().collect::<HashSet<_>>();
    if segments.keys().any(|segment| !valid.contains(segment)) {
        bail!("unknown clock segment")
    }
    Ok(())
}
fn network_url_allowed(policy: &NetworkPolicy, value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid URL")?;
    match url.scheme() {
        "https" => {}
        "http" if policy.allow_insecure_http => {}
        "http" => bail!("HTTP requires network.allow_insecure_http = true"),
        _ => bail!("only HTTP(S) URLs are allowed"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with credentials are not allowed")
    }
    let host = url
        .host_str()
        .context("URL host is required")?
        .to_ascii_lowercase();
    if host == "localhost" {
        bail!("localhost is not allowed")
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let local = match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_unspecified()
                    || ip.is_multicast()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
            }
        };
        if local {
            bail!("local-network addresses are not allowed")
        }
    }
    let allowed = policy.allow.iter().any(|entry| {
        let entry = entry.to_ascii_lowercase();
        entry == "*"
            || entry == host
            || entry
                .strip_prefix("*.")
                .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
    });
    if !allowed {
        bail!("URL host is not allowed by the package manifest")
    }
    Ok(url)
}
fn http_request(
    policy: &NetworkPolicy,
    method: &str,
    url: &str,
    body: Option<String>,
) -> Result<(u16, String)> {
    let url = network_url_allowed(policy, url)?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).context("invalid HTTP method")?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .request(method, url)
        .body(body.unwrap_or_default())
        .send()?;
    let status = response.status().as_u16();
    let mut response_body = String::new();
    response
        .take(1_048_576)
        .read_to_string(&mut response_body)?;
    Ok((status, response_body))
}
const QUASAR_CONFIG: &str = "/system/vendor/quasar/quasar.cfg";
const IPC_MAX_FRAME_SIZE: usize = 10 * 1024 * 1024;
const IPC_RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Extract only `<service>.port` from quasar.cfg; the rest of the file holds secrets.
fn quasar_service_port(config: &str, service: &str) -> Result<Option<u16>> {
    let value: serde_json::Value =
        serde_json::from_str(config).context("invalid quasar config JSON")?;
    Ok(value
        .get(service)
        .and_then(|entry| entry.get("port"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok()))
}
const EV_SYN: u16 = 0;
/// Longest single `poll` wait, so a stopped plugin is noticed within about this long.
const INPUT_POLL_SLICE_MS: u64 = 100;

fn validate_input_policy(m: &Manifest) -> Result<()> {
    if m.input.devices.is_empty() {
        return Ok(());
    }
    if !m.permissions.iter().any(|p| p == "input") {
        bail!("[input].devices requires the input permission")
    }
    if m.input.devices.len() > 16 {
        bail!("[input].devices lists more than 16 devices")
    }
    for name in &m.input.devices {
        if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            bail!("[input].devices contains an invalid device name")
        }
    }
    Ok(())
}
fn validate_ipc_policy(m: &Manifest) -> Result<()> {
    let has_raw_ipc = m
        .permissions
        .iter()
        .any(|permission| matches!(permission.as_str(), "ipc.read" | "ipc.control"));
    if m.ipc.services.is_empty() {
        if has_raw_ipc {
            bail!("ipc.read and ipc.control require a non-empty [ipc].services list")
        }
        return Ok(());
    }
    if !has_raw_ipc {
        bail!("[ipc].services requires ipc.read or ipc.control permission")
    }
    if m.ipc.services.len() > 16 {
        bail!("[ipc].services lists more than 16 services")
    }
    for service in &m.ipc.services {
        if service.is_empty()
            || service.len() > 64
            || !service
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!("[ipc].services contains an invalid service name")
        }
    }
    Ok(())
}
fn normalize_command(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_lowercase().to_string()
            } else {
                " ".into()
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn validate_command_policy(m: &Manifest) -> Result<()> {
    let has_commands = m
        .permissions
        .iter()
        .any(|permission| permission == "commands");
    if m.commands.phrases.is_empty() {
        if has_commands {
            bail!("commands permission requires a non-empty [commands].phrases list")
        }
        return Ok(());
    }
    if !has_commands {
        bail!("[commands].phrases requires the commands permission")
    }
    if m.commands.phrases.len() > 32 {
        bail!("[commands].phrases lists more than 32 phrases")
    }
    for phrase in &m.commands.phrases {
        if phrase.len() > 128 || normalize_command(phrase).is_empty() {
            bail!("[commands].phrases contains an invalid phrase")
        }
    }
    Ok(())
}
fn command_matches(trigger: &str, transcript: &str) -> bool {
    let trigger = normalize_command(trigger);
    let transcript = normalize_command(transcript);
    transcript == trigger
        || transcript
            .strip_prefix("алиса ")
            .is_some_and(|phrase| phrase == trigger)
}
/// The device names a package declared; packages without a list get no input devices.
fn input_scope(m: &Manifest) -> Result<&[String]> {
    if m.input.devices.is_empty() {
        bail!("alice.input.next_event requires a non-empty [input].devices manifest list")
    }
    Ok(&m.input.devices)
}
/// Maps declared device names to `/dev/input/eventN` using `/proc/bus/input/devices` text.
/// Only exact name matches are returned; every declared name must exist.
fn resolve_input_devices(proc_devices: &str, wanted: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if wanted.is_empty() {
        bail!("no input devices declared")
    }
    let mut available = vec![];
    for block in proc_devices.split("\n\n") {
        let mut name = None;
        let mut event = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("N: Name=") {
                name = Some(value.trim().trim_matches('"').to_string());
            } else if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                event = handlers.split_whitespace().find(|handler| {
                    handler
                        .strip_prefix("event")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                });
            }
        }
        if let (Some(name), Some(event)) = (name, event) {
            available.push((name, PathBuf::from("/dev/input").join(event)));
        }
    }
    let mut resolved = vec![];
    for name in wanted {
        let before = resolved.len();
        for (candidate, path) in &available {
            if candidate == name {
                resolved.push((name.clone(), path.clone()));
            }
        }
        if resolved.len() == before {
            bail!("input device {name:?} not found")
        }
    }
    Ok(resolved)
}
#[derive(Debug, Clone, PartialEq)]
struct InputEvent {
    device: String,
    kind: u16,
    code: u16,
    value: i32,
    time_ms: i64,
}
/// Size of the kernel `struct input_event`: two `long` timestamp words plus type, code, value.
fn input_event_size(word: usize) -> usize {
    word * 2 + 8
}
fn read_input_word(bytes: &[u8], word: usize) -> i64 {
    match word {
        4 => i32::from_ne_bytes(bytes[..4].try_into().unwrap()) as i64,
        _ => i64::from_ne_bytes(bytes[..8].try_into().unwrap()),
    }
}
/// Decodes one native-endian `struct input_event` whose timestamp words are `word` bytes wide.
fn decode_input_event(device: &str, bytes: &[u8], word: usize) -> Result<InputEvent> {
    if !matches!(word, 4 | 8) || bytes.len() != input_event_size(word) {
        bail!("malformed input_event record")
    }
    let seconds = read_input_word(bytes, word);
    let micros = read_input_word(&bytes[word..], word);
    let tail = &bytes[word * 2..];
    Ok(InputEvent {
        device: device.to_string(),
        kind: u16::from_ne_bytes([tail[0], tail[1]]),
        code: u16::from_ne_bytes([tail[2], tail[3]]),
        value: i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]),
        time_ms: seconds * 1000 + micros / 1000,
    })
}
struct InputSource {
    name: String,
    file: fs::File,
}
/// Read-only, non-grabbing evdev reader over the devices a package declared.
struct InputReader {
    sources: Vec<InputSource>,
    pending: std::collections::VecDeque<InputEvent>,
}
impl InputReader {
    fn open(devices: &[(String, PathBuf)]) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut sources = vec![];
        for (name, path) in devices {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .with_context(|| format!("open input device {name:?} at {}", path.display()))?;
            sources.push(InputSource {
                name: name.clone(),
                file,
            });
        }
        Ok(Self {
            sources,
            pending: Default::default(),
        })
    }
    /// Waits up to `timeout_ms` for the next non-`EV_SYN` event, checking `cancelled` between
    /// `INPUT_POLL_SLICE_MS` waits.
    fn next(&mut self, timeout_ms: u64, cancelled: &AtomicBool) -> Result<Option<InputEvent>> {
        use std::os::unix::io::AsRawFd;
        let word = std::mem::size_of::<libc::c_long>();
        let size = input_event_size(word);
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut buffer = vec![0u8; size * 32];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("plugin stopped")
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = (remaining.as_millis() as u64).min(INPUT_POLL_SLICE_MS);
            let mut fds: Vec<libc::pollfd> = self
                .sources
                .iter()
                .map(|source| libc::pollfd {
                    fd: source.file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            let ready =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait as i32) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("poll input devices");
                }
            } else if ready > 0 {
                for (source, fd) in self.sources.iter_mut().zip(&fds) {
                    if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        bail!("input device {:?} is no longer available", source.name)
                    }
                    if fd.revents & libc::POLLIN == 0 {
                        continue;
                    }
                    let length = match source.file.read(&mut buffer) {
                        Ok(length) => length,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("read input device {:?}", source.name))
                        }
                    };
                    for record in buffer[..length - length % size].chunks_exact(size) {
                        let event = decode_input_event(&source.name, record, word)?;
                        if event.kind != EV_SYN {
                            self.pending.push_back(event);
                        }
                    }
                }
            }
            if self.pending.is_empty() && std::time::Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}
fn ipc_endpoint(service: &str) -> Result<SocketAddr> {
    if service.is_empty()
        || !service
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("invalid Quasar IPC service name")
    }
    let variable = format!("YAM_{}_ADDR", service.to_ascii_uppercase());
    let address = match std::env::var(&variable) {
        Ok(address) => address,
        Err(_) => {
            let config = std::fs::read_to_string(QUASAR_CONFIG)
                .with_context(|| format!("read {QUASAR_CONFIG} or set {variable}"))?;
            let port = quasar_service_port(&config, service)?
                .with_context(|| format!("{service}.port is not configured"))?;
            format!("127.0.0.1:{port}")
        }
    };
    let endpoint = address
        .parse::<SocketAddr>()
        .context("invalid IPC address")?;
    if !endpoint.ip().is_ipv4() || !endpoint.ip().is_loopback() {
        bail!("Quasar IPC endpoint must be an IPv4 loopback address")
    }
    Ok(endpoint)
}
fn protobuf_varint(data: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*offset).context("truncated protobuf field")?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("invalid protobuf varint")
}
fn protobuf_fields(data: &[u8]) -> Result<Vec<(u32, u8, Vec<u8>)>> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let key = protobuf_varint(data, &mut offset)?;
        let number = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        let value = match wire {
            0 => protobuf_varint(data, &mut offset)?.to_le_bytes().to_vec(),
            1 => {
                let end = offset.checked_add(8).context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            2 => {
                let size = usize::try_from(protobuf_varint(data, &mut offset)?)?;
                let end = offset
                    .checked_add(size)
                    .context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            5 => {
                let end = offset.checked_add(4).context("protobuf length overflow")?;
                let value = data
                    .get(offset..end)
                    .context("truncated protobuf field")?
                    .to_vec();
                offset = end;
                value
            }
            _ => bail!("unsupported protobuf wire type"),
        };
        fields.push((number, wire, value));
    }
    Ok(fields)
}
fn nested_field(data: &[u8], field: u32) -> Result<Option<Vec<u8>>> {
    Ok(protobuf_fields(data)?
        .into_iter()
        .find_map(|(number, wire, value)| (number == field && wire == 2).then_some(value)))
}
fn text_field(data: &[u8], field: u32) -> Result<Option<String>> {
    nested_field(data, field)?
        .map(|value| String::from_utf8(value).context("invalid UTF-8 in IPC message"))
        .transpose()
}
fn varint_field(data: &[u8], field: u32) -> Result<Option<u64>> {
    Ok(protobuf_fields(data)?
        .into_iter()
        .find_map(|(number, wire, value)| {
            (number == field && wire == 0)
                .then(|| value.try_into().ok().map(u64::from_le_bytes))
                .flatten()
        }))
}
/// Unwrap the Midi envelope (field 252 = body, field 253 = QuasarMessage field
/// number) into a plain QuasarMessage; other frames are returned unchanged.
fn normalize_ipc_message(message: &[u8]) -> Result<Vec<u8>> {
    let fields = protobuf_fields(message)?;
    let kind = fields
        .iter()
        .find_map(|(number, wire, value)| (*number == 253 && *wire == 0).then_some(value));
    let body = fields
        .iter()
        .find_map(|(number, wire, value)| (*number == 252 && *wire == 2).then_some(value));
    let (Some(kind), Some(body)) = (kind, body) else {
        return Ok(message.to_vec());
    };
    let kind = u64::from_le_bytes(
        kind.as_slice()
            .try_into()
            .context("invalid envelope type")?,
    );
    let mut out = Vec::with_capacity(body.len() + 12);
    push_varint(&mut out, kind << 3 | 2);
    push_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    Ok(out)
}
fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
fn push_bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    push_varint(out, field << 3 | 2);
    push_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}
/// Sends `AliceCapabilityMethod::CANCEL_DIALOG` through the supported local
/// remoting envelope. This is deliberately exposed only as a fixed command,
/// rather than letting a voice-command package build arbitrary aliced control
/// frames.
fn cancel_alice_dialog() -> Result<()> {
    let mut method = Vec::new();
    push_varint(&mut method, 1 << 3);
    push_varint(&mut method, 3); // AliceCapabilityMethod::CANCEL_DIALOG
    let mut remoting = Vec::new();
    push_bytes(&mut remoting, 7, b"AliceCapability");
    push_bytes(&mut remoting, 8, &method);
    let mut message = Vec::new();
    push_bytes(&mut message, 252, &remoting);
    push_varint(&mut message, 253 << 3);
    push_varint(&mut message, 210); // QuasarMessage.remoting
    ipc_send("aliced", &message)
}
fn decode_ipc_event(service: &str, message: &[u8]) -> Result<Option<serde_json::Value>> {
    let message = normalize_ipc_message(message)?;
    let message = message.as_slice();
    match service {
        "aliced" => {
            let Some(state) = nested_field(message, 3)? else {
                return Ok(None);
            };
            let is_command = match nested_field(&state, 10)? {
                Some(source) => text_field(&source, 6)?.as_deref() == Some("command"),
                None => false,
            };
            if is_command {
                if let Some(request) = nested_field(&state, 15)? {
                    if varint_field(&request, 2)?.unwrap_or(0) != 0 {
                        if let Some(transcript) = text_field(&request, 1)? {
                            return Ok(Some(
                                serde_json::json!({"type": "speech", "source": "command", "transcript": transcript}),
                            ));
                        }
                    }
                }
            }
            Ok(text_field(&state, 2)?.map(|transcript| {
                serde_json::json!({"type": "speech", "source": "voice", "transcript": transcript})
            }))
        }
        "alarmd" => {
            let Some(event) = nested_field(message, 215)? else {
                return Ok(None);
            };
            let Some(alarm) = nested_field(&event, 1)? else {
                return Ok(None);
            };
            Ok(Some(serde_json::json!({
                "type": "alarm_fired",
                "id": text_field(&alarm, 1)?,
                "alarm_type": varint_field(&alarm, 2)?,
                "start_timestamp_ms": varint_field(&alarm, 3)?,
                "duration_seconds": varint_field(&alarm, 4)?,
                "remind_message": text_field(&alarm, 5)?,
                "currently_playing": varint_field(&alarm, 6)?.map(|value| value != 0),
            })))
        }
        _ => bail!("unsupported IPC service"),
    }
}
fn ipc_frame_length(header: [u8; 4]) -> Result<usize> {
    let length = i32::from_le_bytes(header);
    if length < 0 {
        bail!("IPC frame has a negative length")
    }
    let length = length as usize;
    if length > IPC_MAX_FRAME_SIZE {
        bail!("IPC frame exceeds 10 MiB")
    }
    Ok(length)
}

fn validate_quasar_message(payload: &[u8]) -> Result<()> {
    if payload.len() > IPC_MAX_FRAME_SIZE {
        bail!("IPC frame exceeds 10 MiB")
    }
    protobuf_fields(payload).map(|_| ())
}

fn write_ipc_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    validate_quasar_message(payload)?;
    let length = i32::try_from(payload.len()).context("IPC frame is too large")?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

fn read_ipc_frame(stream: &mut TcpStream, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    stream.set_read_timeout(Some(remaining))?;
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    }
    let mut payload = vec![0; ipc_frame_length(header)?];
    stream.read_exact(&mut payload)?;
    validate_quasar_message(&payload)?;
    Ok(Some(payload))
}

fn connect_ipc(endpoint: SocketAddr, deadline: Instant) -> Result<Option<TcpStream>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match TcpStream::connect_timeout(&endpoint, remaining.min(IPC_RECONNECT_DELAY)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(Some(stream));
            }
            Err(_) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(None);
                }
                std::thread::sleep(remaining.min(IPC_RECONNECT_DELAY));
            }
        }
    }
}

fn ipc_send(service: &str, payload: &[u8]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let endpoint = ipc_endpoint(service)?;
    let mut stream =
        connect_ipc(endpoint, deadline)?.context("Quasar IPC service is unavailable")?;
    write_ipc_frame(&mut stream, payload)
}

fn ipc_request(service: &str, payload: &[u8], timeout_ms: u64) -> Result<Option<Vec<u8>>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    validate_quasar_message(payload)?;
    let request_id =
        nested_field(payload, 1)?.context("IPC request must include field 1 request_id")?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let endpoint = ipc_endpoint(service)?;
    let mut stream =
        connect_ipc(endpoint, deadline)?.context("Quasar IPC service is unavailable")?;
    write_ipc_frame(&mut stream, payload)?;
    while let Some(response) = read_ipc_frame(&mut stream, deadline)? {
        if nested_field(&response, 1)?.as_deref() == Some(request_id.as_slice()) {
            return Ok(Some(response));
        }
    }
    Ok(None)
}

fn ipc_receive(service: &str, timeout_ms: u64) -> Result<Option<Vec<u8>>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let endpoint = ipc_endpoint(service)?;
    let Some(mut stream) = connect_ipc(endpoint, deadline)? else {
        return Ok(None);
    };
    read_ipc_frame(&mut stream, deadline)
}

fn ipc_service_allowed(services: &[String], service: &str) -> Result<()> {
    if !services.iter().any(|allowed| allowed == service) {
        bail!("Quasar IPC service is not allowed by [ipc].services")
    }
    Ok(())
}

fn ipc_next(service: &str, timeout_ms: u64) -> Result<Option<serde_json::Value>> {
    if timeout_ms > 60_000 {
        bail!("IPC timeout is limited to 60 seconds")
    }
    let endpoint = ipc_endpoint(service)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    loop {
        let Some(mut stream) = connect_ipc(endpoint, deadline)? else {
            return Ok(None);
        };
        let mut header = [0u8; 4];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            stream.set_read_timeout(Some(remaining))?;
            match stream.read_exact(&mut header) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None)
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    break
                }
                Err(error) => return Err(error.into()),
            }
            let mut payload = vec![0; ipc_frame_length(header)?];
            if let Err(error) = stream.read_exact(&mut payload) {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::ConnectionReset
                ) {
                    break;
                }
                return Err(error.into());
            }
            // A successful wire-level parse is the minimum validation for a
            // proto2 QuasarMessage; unknown fields are intentionally retained.
            protobuf_fields(&payload)?;
            if let Some(event) = decode_ipc_event(service, &payload)? {
                return Ok(Some(event));
            }
        }
    }
}

fn clock_time(value: &str, brightness: u8) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![bytes[0], bytes[1], bytes[3], bytes[4]]
            .iter()
            .all(u8::is_ascii_digit)
    {
        bail!("clock time must have HH:MM format")
    }
    let digits: HashMap<u8, &[&str]> = HashMap::from([
        (b'0', &["A", "B", "C", "D", "E", "F"] as &[&str]),
        (b'1', &["B", "C"]),
        (b'2', &["A", "B", "G", "E", "D"]),
        (b'3', &["A", "B", "C", "D", "G"]),
        (b'4', &["F", "G", "B", "C"]),
        (b'5', &["A", "F", "G", "C", "D"]),
        (b'6', &["A", "F", "G", "E", "C", "D"]),
        (b'7', &["A", "B", "C"]),
        (b'8', &["A", "B", "C", "D", "E", "F", "G"]),
        (b'9', &["A", "B", "C", "D", "F", "G"]),
    ]);
    let mut segments = HashMap::new();
    let layout = clock_layout().context("segment clock is not available")?;
    // plum numbers its positions from 0 and has no PLUS/MINUS markers: its
    // colon is the upper and lower halves of the separator.
    let base = if matches!(layout, ClockLayout::Plum(_)) {
        0
    } else {
        1
    };
    for (index, digit) in [bytes[0], bytes[1], bytes[3], bytes[4]].iter().enumerate() {
        for part in digits.get(digit).context("invalid clock digit")?.iter() {
            segments.insert(format!("{part}{}", index + base), brightness);
        }
    }
    match layout {
        ClockLayout::Plum(_) => {
            segments.insert("DH".into(), brightness);
            segments.insert("DL".into(), brightness);
        }
        _ => {
            segments.insert("PLUS1".into(), brightness);
            segments.insert("PLUS2".into(), brightness);
        }
    }
    clock_write(&segments)
}
fn validate_id(s: &str) -> Result<()> {
    if s.is_empty()
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid plugin id")
    };
    Ok(())
}
fn safe_relative(s: &str) -> Result<()> {
    let p = Path::new(s);
    if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("unsafe path")
    };
    Ok(())
}
fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}
fn command_output(program: &str, args: &[String]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
fn volume_percent(raw: &str) -> Option<u8> {
    raw.split_whitespace()
        .filter_map(|part| part.find('%').map(|index| &part[..index]))
        .filter_map(|value| {
            value
                .trim_matches(|character: char| !character.is_ascii_digit())
                .parse::<u8>()
                .ok()
        })
        .next()
}
fn volume_info() -> Result<VolumeInfo> {
    let raw = command_output("amixer", &["get".into(), "Master".into()])?;
    Ok(VolumeInfo {
        percent: volume_percent(&raw),
        raw,
    })
}
fn set_volume(percent: u8) -> Result<VolumeInfo> {
    if percent > 100 {
        bail!("volume must be 0..100")
    }
    command_output(
        "amixer",
        &["set".into(), "Master".into(), format!("{percent}%")],
    )?;
    volume_info()
}
fn valid_mac(mac: &str) -> bool {
    mac.len() == 17
        && mac.bytes().enumerate().all(|(i, b)| {
            if [2, 5, 8, 11, 14].contains(&i) {
                b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn configured_rgb_ring(settings: &LedSettings) -> bool {
    ["red", "green", "blue"]
        .iter()
        .all(|channel| led_brightness_path(settings, channel, 0).is_ok_and(|p| p.exists()))
}
/// Returns whether the named speaker API is usable on this device. This is a
/// capability probe, not a permission check: an unavailable API returns false,
/// while an API omitted from a package's manifest remains inaccessible.
fn lua_api_available(device: &DeviceProfile, name: &str) -> Result<bool> {
    let has_feature = |feature: &str| device.features.iter().any(|value| value == feature);
    let quasar_has = |service: &str| {
        fs::read_to_string(QUASAR_CONFIG)
            .ok()
            .and_then(|config| quasar_service_port(&config, service).ok().flatten())
            .is_some()
    };
    Ok(match name {
        "led" => device
            .features
            .iter()
            .any(|feature| feature.starts_with("led.")),
        "volume" => has_feature("audio.volume.alsa"),
        "audio" => has_feature("audio.play.aplay"),
        "bluetooth" => has_feature("bluetooth.bluez"),
        "display" => has_feature("display.backlight"),
        "sensors" => device
            .features
            .iter()
            .any(|feature| feature.starts_with("sensor.")),
        "sensors.control" => has_feature("sensor.ambient_light.iio"),
        "clock" => has_feature("display.clock.segment.v1"),
        "input" => has_feature("input.evdev"),
        "network" | "system" => true,
        "ipc.alarm" => quasar_has("alarmd"),
        "ipc.speech" | "commands" => quasar_has("aliced"),
        "ipc.read" | "ipc.control" => Path::new(QUASAR_CONFIG).is_file(),
        "exec" => bail!("exec has no is_available probe"),
        _ => bail!("unknown speaker API {name:?}"),
    })
}

async fn run_plugin(app: App, id: String, event: String, cancelled: Arc<AtomicBool>) -> Result<()> {
    let device = app.device.lock().await.clone();
    let led_count = app.led.settings.lock().await.count;
    let (manifest, root, allowed) = {
        let p = app.plugins.lock().await;
        if !p.enabled.contains(&id) {
            bail!("plugin is disabled")
        };
        let m = p.manifest(&id)?;
        if let Some(reason) = compatibility_error(&m, &device) {
            bail!("plugin is incompatible: {reason}")
        };
        (
            m,
            p.root.join(&id),
            app.settings.lock().await.allow_plugin_binaries,
        )
    };
    let led = app.led.clone();
    task::spawn_blocking(move || -> Result<()> {
        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )?;
        let globals = lua.globals();
        let api = lua.create_table()?;
        let permissions: HashSet<_> = manifest.permissions.iter().map(String::as_str).collect();
        let available_device = device.clone();
        api.set(
            "is_available",
            lua.create_function(move |_, name: String| {
                lua_api_available(&available_device, &name).map_err(mlua::Error::external)
            })?,
        )?;
        let model = device.model.clone();
        api.set(
            "model",
            lua.create_function(move |_, ()| Ok(model.clone()))?,
        )?;
        let cancelled_sleep = cancelled.clone();
        api.set(
            "sleep",
            lua.create_function(move |_, ms: u64| {
                if ms > 60_000 {
                    return Err(mlua::Error::external("sleep is limited to 60 seconds"));
                };
                let mut elapsed = 0;
                while elapsed < ms {
                    if cancelled_sleep.load(Ordering::Relaxed) {
                        return Err(mlua::Error::external("plugin stopped"));
                    }
                    let slice = (ms - elapsed).min(20);
                    std::thread::sleep(Duration::from_millis(slice));
                    elapsed += slice;
                }
                Ok(())
            })?,
        )?;
        if permissions.contains("led") {
            let led2 = led.clone();
            let cancelled_set = cancelled.clone();
            api.set(
                "set",
                lua.create_function(move |_, colors: mlua::Table| {
                    if cancelled_set.load(Ordering::Relaxed) {
                        return Err(mlua::Error::external("plugin stopped"));
                    }
                    let v = colors
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    tokio::runtime::Handle::current()
                        .block_on(led2.draw(v))
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let led3 = led.clone();
            api.set(
                "off",
                lua.create_function(move |_, ()| {
                    tokio::runtime::Handle::current()
                        .block_on(led3.off())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            let pattern_root = root.clone();
            let cancelled_play = cancelled.clone();
            api.set(
                "play",
                lua.create_function(move |_, name: String| {
                    safe_relative(&name).map_err(mlua::Error::external)?;
                    tokio::runtime::Handle::current()
                        .block_on(led.play_led(pattern_root.join(name), &cancelled_play))
                        .map_err(mlua::Error::external)
                })?,
            )?;
        }
        if permissions.contains("volume") {
            let volume = lua.create_table()?;
            volume.set(
                "set",
                lua.create_function(|_, percent: u8| {
                    if percent > 100 {
                        return Err(mlua::Error::external("volume must be 0..100"));
                    };
                    command_output(
                        "amixer",
                        &["set".into(), "Master".into(), format!("{percent}%")],
                    )
                    .map_err(mlua::Error::external)
                })?,
            )?;
            volume.set(
                "get",
                lua.create_function(|_, ()| {
                    command_output("amixer", &["get".into(), "Master".into()])
                        .map_err(mlua::Error::external)
                })?,
            )?;
            api.set("volume", volume)?;
        }
        if permissions.contains("audio") {
            let audio = lua.create_table()?;
            let audio_root = root.clone();
            audio.set(
                "play",
                lua.create_function(move |_, file: String| {
                    safe_relative(&file).map_err(mlua::Error::external)?;
                    let path = audio_root.join(file);
                    if !path.is_file() {
                        return Err(mlua::Error::external("audio file not found"));
                    };
                    Command::new("aplay")
                        .arg("-q")
                        .arg(path)
                        .spawn()
                        .map(|mut child| {
                            std::thread::spawn(move || child.wait());
                        })
                        .map_err(mlua::Error::external)
                })?,
            )?;
            api.set("audio", audio)?;
        }
        if permissions.contains("bluetooth") {
            let bt = lua.create_table()?;
            bt.set(
                "status",
                lua.create_function(|_, ()| {
                    command_output("bluetoothctl", &["show".into()]).map_err(mlua::Error::external)
                })?,
            )?;
            bt.set(
                "power",
                lua.create_function(|_, on: bool| {
                    command_output(
                        "bluetoothctl",
                        &["power".into(), if on { "on".into() } else { "off".into() }],
                    )
                    .map_err(mlua::Error::external)
                })?,
            )?;
            bt.set(
                "scan",
                lua.create_function(|_, on: bool| {
                    command_output(
                        "bluetoothctl",
                        &["scan".into(), if on { "on".into() } else { "off".into() }],
                    )
                    .map_err(mlua::Error::external)
                })?,
            )?;
            for action in ["pair", "connect", "disconnect", "remove"] {
                let a = action.to_string();
                bt.set(
                    action,
                    lua.create_function(move |_, mac: String| {
                        if !valid_mac(&mac) {
                            return Err(mlua::Error::external("invalid Bluetooth MAC"));
                        };
                        command_output("bluetoothctl", &[a.clone(), mac])
                            .map_err(mlua::Error::external)
                    })?,
                )?;
            }
            api.set("bluetooth", bt)?;
        }
        if permissions.contains("display") {
            let display = lua.create_table()?;
            display.set(
                "backlights",
                lua.create_function(|lua, ()| {
                    let t = lua.create_table()?;
                    if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
                        for (i, e) in entries.flatten().enumerate() {
                            t.set(i + 1, e.file_name().to_string_lossy().to_string())?
                        }
                    }
                    Ok(t)
                })?,
            )?;
            display.set(
                "set_backlight",
                lua.create_function(|_, (name, value): (String, u32)| {
                    validate_id(&name).map_err(mlua::Error::external)?;
                    let base = Path::new("/sys/class/backlight").join(&name);
                    let max: u32 = fs::read_to_string(base.join("max_brightness"))
                        .map_err(mlua::Error::external)?
                        .trim()
                        .parse()
                        .map_err(mlua::Error::external)?;
                    if value > max {
                        return Err(mlua::Error::external("brightness exceeds max_brightness"));
                    };
                    fs::write(base.join("brightness"), value.to_string())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            api.set("display", display)?;
        }
        if permissions.contains("sensors") {
            let sensors = lua.create_table()?;
            sensors.set(
                "list",
                lua.create_function(|lua, ()| {
                    let result = lua.create_table()?;
                    for (index, sensor) in discover_sensors().iter().enumerate() {
                        let item = lua.create_table()?;
                        item.set("id", sensor.id.as_str())?;
                        item.set("kind", sensor.kind.as_str())?;
                        item.set("unit", sensor.unit.as_str())?;
                        result.set(index + 1, item)?;
                    }
                    Ok(result)
                })?,
            )?;
            sensors.set(
                "read",
                lua.create_function(|lua, id: String| {
                    let sensor = discover_sensors()
                        .into_iter()
                        .find(|sensor| sensor.id == id)
                        .context("sensor not found")
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("id", sensor.id.as_str())?;
                    result.set("kind", sensor.kind.as_str())?;
                    result.set("unit", sensor.unit.as_str())?;
                    result.set(
                        "value",
                        sensor_value(&sensor).map_err(mlua::Error::external)?,
                    )?;
                    Ok(result)
                })?,
            )?;
            if permissions.contains("sensors.control") {
                sensors.set(
                    "configure_ambient_light",
                    lua.create_function(|_, (name, gain, integration_ms): (String, u8, u16)| {
                        if ![1, 3, 6, 9, 18].contains(&gain) {
                            return Err(mlua::Error::external("gain must be 1, 3, 6, 9 or 18"));
                        }
                        if ![25, 50, 100, 200, 400].contains(&integration_ms) {
                            return Err(mlua::Error::external(
                                "integration time must be 25, 50, 100, 200 or 400 ms",
                            ));
                        }
                        let path = configured_light_sensor(&name).map_err(mlua::Error::external)?;
                        fs::write(path.join("in_intensity_hardwaregain"), gain.to_string())
                            .map_err(mlua::Error::external)?;
                        fs::write(
                            path.join("in_intensity_integration_time"),
                            format!("0.{integration_ms:03}"),
                        )
                        .map_err(mlua::Error::external)
                    })?,
                )?;
            }
            api.set("sensors", sensors)?;
        }
        if permissions.contains("clock") {
            let clock = lua.create_table()?;
            clock.set(
                "available",
                lua.create_function(|_, ()| Ok(clock_available()))?,
            )?;
            clock.set(
                "segments",
                lua.create_function(|_, levels: mlua::Table| {
                    let mut frame = HashMap::new();
                    for pair in levels.pairs::<String, u8>() {
                        let (segment, value) = pair?;
                        frame.insert(segment, value);
                    }
                    validate_clock_segments(&frame).map_err(mlua::Error::external)?;
                    clock_write(&frame).map_err(mlua::Error::external)
                })?,
            )?;
            clock.set(
                "time",
                lua.create_function(|_, (time, brightness): (String, Option<u8>)| {
                    clock_time(&time, brightness.unwrap_or(170)).map_err(mlua::Error::external)
                })?,
            )?;
            clock.set(
                "off",
                lua.create_function(|_, ()| clock_release().map_err(mlua::Error::external))?,
            )?;
            api.set("clock", clock)?;
        }
        if permissions.contains("input") {
            let input = lua.create_table()?;
            input.set(
                "devices",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/bus/input/devices").map_err(mlua::Error::external)
                })?,
            )?;
            let input_manifest = manifest.clone();
            let cancelled_input = cancelled.clone();
            let mut reader: Option<InputReader> = None;
            input.set(
                "next_event",
                lua.create_function_mut(move |lua, timeout_ms: u64| {
                    if timeout_ms > 60_000 {
                        return Err(mlua::Error::external(
                            "input timeout is limited to 60 seconds",
                        ));
                    }
                    if reader.is_none() {
                        let opened = input_scope(&input_manifest)
                            .and_then(|names| {
                                let listing = fs::read_to_string("/proc/bus/input/devices")
                                    .context("read /proc/bus/input/devices")?;
                                resolve_input_devices(&listing, names)
                            })
                            .and_then(|devices| InputReader::open(&devices))
                            .map_err(mlua::Error::external)?;
                        reader = Some(opened);
                    }
                    let event = reader
                        .as_mut()
                        .expect("reader opened above")
                        .next(timeout_ms, &cancelled_input)
                        .map_err(mlua::Error::external)?;
                    match event {
                        Some(event) => {
                            let table = lua.create_table()?;
                            table.set("device", event.device)?;
                            table.set("type", event.kind)?;
                            table.set("code", event.code)?;
                            table.set("value", event.value)?;
                            table.set("time_ms", event.time_ms)?;
                            Ok(mlua::Value::Table(table))
                        }
                        None => Ok(mlua::Value::Nil),
                    }
                })?,
            )?;
            api.set("input", input)?;
        }
        if permissions.contains("network") {
            if manifest.network.allow.is_empty() {
                bail!("network permission requires a non-empty [network].allow manifest list")
            }
            let http = lua.create_table()?;
            let get_policy = manifest.network.clone();
            http.set(
                "get",
                lua.create_function(move |lua, url: String| {
                    let (status, body) = http_request(&get_policy, "GET", &url, None)
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("body", body)?;
                    Ok(result)
                })?,
            )?;
            let post_policy = manifest.network.clone();
            http.set(
                "post",
                lua.create_function(move |lua, (url, body): (String, String)| {
                    if body.len() > 1_048_576 {
                        return Err(mlua::Error::external("request body exceeds 1 MiB"));
                    }
                    let (status, body) = http_request(&post_policy, "POST", &url, Some(body))
                        .map_err(mlua::Error::external)?;
                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("body", body)?;
                    Ok(result)
                })?,
            )?;
            api.set("http", http)?;
        }
        if permissions.contains("ipc.alarm")
            || permissions.contains("ipc.speech")
            || permissions.contains("ipc.read")
            || permissions.contains("ipc.control")
        {
            let ipc = lua.create_table()?;
            if permissions.contains("ipc.alarm") {
                ipc.set(
                    "next_alarm",
                    lua.create_function(|lua, timeout_ms: u64| {
                        match ipc_next("alarmd", timeout_ms).map_err(mlua::Error::external)? {
                            Some(event) => lua.to_value(&event),
                            None => Ok(mlua::Value::Nil),
                        }
                    })?,
                )?;
            }
            if permissions.contains("ipc.speech") {
                ipc.set(
                    "next_speech",
                    lua.create_function(|lua, timeout_ms: u64| {
                        match ipc_next("aliced", timeout_ms).map_err(mlua::Error::external)? {
                            Some(event) => lua.to_value(&event),
                            None => Ok(mlua::Value::Nil),
                        }
                    })?,
                )?;
            }
            if permissions.contains("ipc.read") || permissions.contains("ipc.control") {
                if manifest.ipc.services.is_empty() {
                    bail!("ipc.read and ipc.control require a non-empty [ipc].services list")
                }
            }
            if permissions.contains("ipc.read") {
                let read_services = manifest.ipc.services.clone();
                ipc.set(
                    "next",
                    lua.create_function(move |lua, (service, timeout_ms): (String, u64)| {
                        ipc_service_allowed(&read_services, &service)
                            .and_then(|_| ipc_receive(&service, timeout_ms))
                            .map_err(mlua::Error::external)?
                            .map(|payload| lua.create_string(&payload))
                            .transpose()?
                            .map(mlua::Value::String)
                            .map_or(Ok(mlua::Value::Nil), Ok)
                    })?,
                )?;
            }
            if permissions.contains("ipc.control") {
                let control_services = manifest.ipc.services.clone();
                ipc.set(
                    "send",
                    lua.create_function(move |_, (service, payload): (String, mlua::String)| {
                        ipc_service_allowed(&control_services, &service)
                            .and_then(|_| ipc_send(&service, payload.as_bytes().as_ref()))
                            .map_err(mlua::Error::external)
                    })?,
                )?;
            }
            if permissions.contains("ipc.read") && permissions.contains("ipc.control") {
                let request_services = manifest.ipc.services.clone();
                ipc.set(
                    "request",
                    lua.create_function(
                        move |lua, (service, payload, timeout_ms): (String, mlua::String, u64)| {
                            ipc_service_allowed(&request_services, &service)
                                .and_then(|_| {
                                    ipc_request(&service, payload.as_bytes().as_ref(), timeout_ms)
                                })
                                .map_err(mlua::Error::external)?
                                .map(|response| lua.create_string(&response))
                                .transpose()?
                                .map(mlua::Value::String)
                                .map_or(Ok(mlua::Value::Nil), Ok)
                        },
                    )?,
                )?;
            }
            api.set("ipc", ipc)?;
        }
        if permissions.contains("commands") {
            let command = lua.create_table()?;
            command.set(
                "cancel",
                lua.create_function(|_, ()| cancel_alice_dialog().map_err(mlua::Error::external))?,
            )?;
            let allowed_phrases = manifest.commands.phrases.clone();
            let command_cancelled = cancelled.clone();
            command.set(
                "listen",
                lua.create_function(
                    move |lua, (phrase, handler): (String, mlua::Function)| -> mlua::Result<()> {
                        if !allowed_phrases
                            .iter()
                            .any(|allowed| normalize_command(allowed) == normalize_command(&phrase))
                        {
                            return Err(mlua::Error::external(
                                "voice command is not declared in [commands].phrases",
                            ));
                        }
                        loop {
                            if command_cancelled.load(Ordering::Relaxed) {
                                return Err(mlua::Error::external("plugin stopped"));
                            }
                            let Some(event) =
                                ipc_next("aliced", 1_000).map_err(mlua::Error::external)?
                            else {
                                continue;
                            };
                            let Some(transcript) = event["transcript"].as_str() else {
                                continue;
                            };
                            if !command_matches(&phrase, transcript) {
                                continue;
                            }
                            let command_event = lua.create_table()?;
                            command_event.set("transcript", transcript)?;
                            command_event.set("command", normalize_command(&phrase))?;
                            command_event
                                .set("source", event["source"].as_str().unwrap_or("voice"))?;
                            handler.call::<()>(command_event)?;
                        }
                    },
                )?,
            )?;
            api.set("command", command)?;
        }
        if permissions.contains("system") {
            let system = lua.create_table()?;
            system.set(
                "hostname",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/etc/hostname")
                        .map(|x| x.trim().to_string())
                        .map_err(mlua::Error::external)
                })?,
            )?;
            system.set(
                "uptime",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/uptime").map_err(mlua::Error::external)
                })?,
            )?;
            system.set(
                "meminfo",
                lua.create_function(|_, ()| {
                    fs::read_to_string("/proc/meminfo").map_err(mlua::Error::external)
                })?,
            )?;
            api.set("system", system)?;
        }
        if permissions.contains("exec") {
            let plugin_root = root.clone();
            api.set(
                "exec",
                lua.create_function(move |_, (program, args): (String, Vec<String>)| {
                    if !allowed {
                        return Err(mlua::Error::external(
                            "binary execution disabled by administrator",
                        ));
                    };
                    safe_relative(&program).map_err(mlua::Error::external)?;
                    let output = Command::new(plugin_root.join(program))
                        .args(args)
                        .current_dir(&plugin_root)
                        .output()
                        .map_err(mlua::Error::external)?;
                    Ok(String::from_utf8_lossy(&output.stdout).to_string())
                })?,
            )?;
        }
        globals.set("alice", api)?;
        globals.set("event", event)?;
        let lua_device = lua.create_table()?;
        lua_device.set("model", device.model)?;
        lua_device.set("architecture", device.architecture)?;
        lua_device.set("led_count", led_count)?;
        let lua_features = lua.create_table()?;
        for (i, feature) in device.features.iter().enumerate() {
            lua_features.set(i + 1, feature.as_str())?;
        }
        lua_device.set("features", lua_features)?;
        globals.set("device", lua_device)?;
        let launch_path = manifest.launch.path(&manifest).to_string();
        let source = fs::read_to_string(root.join(&launch_path))?;
        lua.load(&source).set_name(&launch_path).exec()?;
        Ok(())
    })
    .await??;
    Ok(())
}

async fn run_binary(
    app: App,
    id: String,
    invocation_args: Vec<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let device = app.device.lock().await.clone();
    let (manifest, root) = {
        let plugins = app.plugins.lock().await;
        if !plugins.enabled.contains(&id) {
            bail!("package is disabled")
        }
        let manifest = plugins.manifest(&id)?;
        if let Some(reason) = compatibility_error(&manifest, &device) {
            bail!("package is incompatible: {reason}")
        }
        if !manifest
            .permissions
            .iter()
            .any(|permission| permission == "exec")
        {
            bail!("binary packages require the exec permission")
        }
        (manifest, plugins.root.join(&id))
    };
    let token = Uuid::new_v4().to_string();
    app.runtime_tokens
        .lock()
        .await
        .insert(id.clone(), token.clone());
    let api_url = format!("{}/api/runtime/{id}", app.runtime_api_url);
    let task_id = id.clone();
    let result = task::spawn_blocking(move || -> Result<()> {
        let executable = manifest.launch.path(&manifest).to_string();
        safe_relative(&executable)?;
        let log = Arc::new(std::sync::Mutex::new(RotatingLog::open(
            root.join("runtime.log"),
        )?));
        let (interpreter, program) = launch_program(&manifest, &root, &executable);
        let mut command = match interpreter {
            Some(interpreter) => {
                let mut command = Command::new(interpreter);
                command.arg(program);
                command
            }
            None => Command::new(program),
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .args(&manifest.launch.args)
            .args(invocation_args)
            .current_dir(&root)
            .env("LD_LIBRARY_PATH", binary_library_path(&manifest, &root))
            .env("YAM_API_URL", api_url)
            .env("YAM_API_TOKEN", token)
            .env("YAM_PLUGIN_ID", task_id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pumps = [
            pump_log(child.stdout.take(), log.clone()),
            pump_log(child.stderr.take(), log),
        ];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                stop_process_group(&mut child);
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                for _ in 0..20 {
                    if pumps.iter().all(std::thread::JoinHandle::is_finished) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if status.success() {
                    return Ok(());
                }
                bail!("binary exited with {status}")
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })
    .await?;
    app.runtime_tokens.lock().await.remove(&id);
    result
}

/// A binary package runs as the leader of its own process group, so stopping it
/// also reaches helpers it started (`aplay`, `arecord`, shell pipelines) instead of
/// leaving them orphaned. The group gets SIGTERM first to let the package clean up.
fn stop_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = -(child.id() as libc::pid_t);
        unsafe { libc::kill(group, libc::SIGTERM) };
        for _ in 0..40 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

// Packages converted before `launch.interpreter` / `launch.library_paths`
// existed carry an apkg `<id>/<id>-run` script and `<prefix>/lib` next to
// `<prefix>/bin`; both are only guessed when the manifest is silent.
fn launch_program(
    manifest: &Manifest,
    root: &Path,
    executable: &str,
) -> (Option<&'static str>, PathBuf) {
    if manifest.launch.interpreter.as_deref() == Some("sh") {
        return (Some("sh"), root.join(executable));
    }
    let legacy_runner = root.join(&manifest.id).join(format!("{}-run", manifest.id));
    if manifest.launch.interpreter.is_none() && legacy_runner.is_file() {
        return (Some("sh"), legacy_runner);
    }
    (None, root.join(executable))
}
struct RotatingLog {
    path: PathBuf,
    file: fs::File,
    size: u64,
}
impl RotatingLog {
    const LIMIT: u64 = 1_048_576;
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self { path, file, size })
    }
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if self.size > Self::LIMIT {
            fs::rename(&self.path, self.path.with_extension("log.1"))?;
            *self = Self::open(self.path.clone())?;
        }
        self.file.write_all(data)?;
        self.size += data.len() as u64;
        Ok(())
    }
}
fn pump_log(
    source: Option<impl Read + Send + 'static>,
    log: Arc<std::sync::Mutex<RotatingLog>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut source) = source else {
            return;
        };
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = source.read(&mut buffer) {
            let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
            if log.write(&buffer[..read]).is_err() {
                return;
            }
        }
    })
}
fn binary_library_path(manifest: &Manifest, root: &Path) -> String {
    let bundled: Vec<PathBuf> = if manifest.launch.library_paths.is_empty() {
        Path::new(manifest.launch.path(manifest))
            .parent()
            .and_then(Path::parent)
            .map(|directory| root.join(directory).join("lib"))
            .filter(|directory| directory.is_dir())
            .into_iter()
            .collect()
    } else {
        manifest
            .launch
            .library_paths
            .iter()
            .map(|directory| root.join(directory))
            .collect()
    };
    let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    bundled
        .iter()
        .map(|directory| directory.display().to_string())
        .chain((!existing.is_empty()).then_some(existing))
        .collect::<Vec<_>>()
        .join(":")
}

async fn run_package(
    app: App,
    id: String,
    invocation_args: Vec<String>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let kind = app.plugins.lock().await.manifest(&id)?.launch.kind;
    match kind {
        LaunchKind::Lua => run_plugin(app, id, "manual".into(), cancelled).await,
        LaunchKind::Binary => run_binary(app, id, invocation_args, cancelled).await,
    }
}

async fn start_package(app: App, id: String, invocation_args: Vec<String>) -> Result<()> {
    let cancelled = {
        let mut plugins = app.plugins.lock().await;
        if !plugins.enabled.contains(&id) {
            bail!("package is disabled")
        }
        let manifest = plugins.manifest(&id)?;
        if manifest.launch.kind == LaunchKind::Binary
            && !app.settings.lock().await.allow_plugin_binaries
        {
            bail!("binary execution disabled by administrator")
        }
        if !invocation_args.is_empty() {
            if manifest.launch.kind != LaunchKind::Binary {
                bail!("only binary packages accept invocation arguments")
            }
        }
        if let Some(previous) = plugins.running.remove(&id) {
            previous.store(true, Ordering::Relaxed)
        }
        let token = Arc::new(AtomicBool::new(false));
        plugins.running.insert(id.clone(), token.clone());
        token
    };
    let task_app = app.clone();
    let task_id = id.clone();
    tokio::spawn(async move {
        if let Err(error) = run_package(
            task_app.clone(),
            task_id.clone(),
            invocation_args,
            cancelled.clone(),
        )
        .await
        {
            if !cancelled.load(Ordering::Relaxed) {
                tracing::error!(package = %task_id, %error, "package execution failed")
            }
        }
        let mut plugins = task_app.plugins.lock().await;
        if plugins
            .running
            .get(&task_id)
            .is_some_and(|current| Arc::ptr_eq(current, &cancelled))
        {
            plugins.running.remove(&task_id);
        }
    });
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(UI)
}
async fn list(State(a): State<App>) -> Result<Json<Vec<PluginInfo>>, ApiError> {
    let d = a.device.lock().await.clone();
    Ok(Json(a.plugins.lock().await.list(&d)?))
}
async fn install(State(a): State<App>, mut form: Multipart) -> Result<Json<Manifest>, ApiError> {
    while let Some(f) = form.next_field().await? {
        if f.name() == Some("package") {
            let b = f.bytes().await?;
            let m = a.plugins.lock().await.install(&b)?;
            return Ok(Json(m));
        }
    }
    Err(ApiError(anyhow!("missing package field")))
}

#[derive(Deserialize)]
struct RegistryInstall {
    id: String,
    version: Option<String>,
    registry: String,
}

async fn install_registry(
    State(a): State<App>,
    Json(request): Json<RegistryInstall>,
) -> Result<Json<Manifest>, ApiError> {
    let device = a.device.lock().await.clone();
    let package = task::spawn_blocking(move || {
        download_registry_package(
            &request.registry,
            &request.id,
            request.version.as_deref(),
            &device,
        )
    })
    .await
    .context("registry download task failed")??;
    let manifest = a.plugins.lock().await.install(&package)?;
    Ok(Json(manifest))
}

fn download_registry_package(
    registry: &str,
    id: &str,
    version: Option<&str>,
    device: &DeviceProfile,
) -> Result<Vec<u8>> {
    let index_url = if registry.trim_end_matches('/').ends_with("index.json") {
        registry.to_string()
    } else {
        format!("{}/index.json", registry.trim_end_matches('/'))
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let index: serde_json::Value = client.get(&index_url).send()?.error_for_status()?.json()?;
    let packages = index["packages"]
        .as_array()
        .context("invalid registry package index")?;
    let entry = packages
        .iter()
        .filter(|entry| entry["id"] == id)
        .filter(|entry| version.is_none_or(|version| entry["version"] == version))
        .filter(|entry| registry_compatible(entry, device))
        .max_by(|left, right| {
            registry_version(
                left["version"].as_str().unwrap_or(""),
                right["version"].as_str().unwrap_or(""),
            )
            .then_with(|| {
                registry_target_rank(left, device).cmp(&registry_target_rank(right, device))
            })
        })
        .with_context(|| {
            format!(
                "no package build for {} matches device {}",
                id, device.model
            )
        })?;
    let package_url = entry["url"]
        .as_str()
        .context("registry entry has no package URL")?;
    let package_url = Url::parse(package_url)
        .or_else(|_| Url::parse(&index_url)?.join(package_url))?
        .to_string();
    let package = client
        .get(package_url)
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    let expected = entry["sha256"]
        .as_str()
        .context("registry entry has no SHA-256")?;
    if format!("{:x}", Sha256::digest(&package)) != expected {
        bail!("package checksum mismatch")
    }
    Ok(package)
}
fn registry_values<'a>(entry: &'a serde_json::Value, key: &str) -> Vec<&'a str> {
    entry[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect()
}
fn registry_compatible(entry: &serde_json::Value, device: &DeviceProfile) -> bool {
    let models = registry_values(entry, "models");
    let features = registry_values(entry, "features");
    (models.is_empty() || models.contains(&device.model.as_str()))
        && features
            .iter()
            .all(|feature| device.features.iter().any(|available| available == feature))
}
fn registry_target_rank(entry: &serde_json::Value, device: &DeviceProfile) -> u8 {
    u8::from(registry_values(entry, "models").contains(&device.model.as_str()))
}

fn registry_version(left: &str, right: &str) -> std::cmp::Ordering {
    let numbers = |value: &str| {
        value
            .trim_start_matches('v')
            .split(|character: char| character == '.' || character == '-')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let left_numbers = numbers(left);
    let right_numbers = numbers(right);
    for index in 0..left_numbers.len().max(right_numbers.len()) {
        match left_numbers
            .get(index)
            .copied()
            .unwrap_or(0)
            .cmp(&right_numbers.get(index).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    left.cmp(right)
}
async fn toggle(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let mut p = a.plugins.lock().await;
    let enabled = p.enabled.insert(id.clone());
    if !enabled {
        p.enabled.remove(&id);
        p.service_enabled.remove(&id);
        if let Some(token) = p.running.remove(&id) {
            token.store(true, Ordering::Relaxed);
        }
    }
    p.save()?;
    drop(p);
    Ok(StatusCode::NO_CONTENT)
}
async fn remove(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    a.plugins.lock().await.remove(&id)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn invoke(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    invocation: Option<Json<Invocation>>,
) -> Result<StatusCode, ApiError> {
    start_package(
        a,
        id,
        invocation.map(|value| value.0.args).unwrap_or_default(),
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}
async fn start_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    start_package(a, id, Vec::new()).await?;
    Ok(StatusCode::ACCEPTED)
}
async fn enable_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    {
        let mut plugins = a.plugins.lock().await;
        plugins.manifest(&id)?;
        plugins.enabled.insert(id.clone());
        plugins.service_enabled.insert(id.clone());
        plugins.save()?;
    }
    start_package(a, id, Vec::new()).await?;
    Ok(StatusCode::ACCEPTED)
}
async fn disable_service(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let mut plugins = a.plugins.lock().await;
    plugins.manifest(&id)?;
    plugins.service_enabled.remove(&id);
    if let Some(token) = plugins.running.remove(&id) {
        token.store(true, Ordering::Relaxed);
    }
    plugins.save()?;
    Ok(StatusCode::NO_CONTENT)
}
async fn stop_plugin(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let mut plugins = a.plugins.lock().await;
    let Some(token) = plugins.running.remove(&id) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    token.store(true, Ordering::Relaxed);
    let uses_led = plugins
        .manifest(&id)?
        .permissions
        .iter()
        .any(|p| p == "led");
    drop(plugins);
    if uses_led {
        let _ = a.led.off().await;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn package_logs(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
) -> Result<String, ApiError> {
    validate_id(&id)?;
    let path = a.plugins.lock().await.root.join(id).join("runtime.log");
    Ok(fs::read_to_string(path).unwrap_or_default())
}
async fn runtime_permission(
    app: &App,
    id: &str,
    headers: &HeaderMap,
    permission: &str,
) -> Result<(), ApiError> {
    validate_id(id)?;
    let token = headers
        .get("x-yam-token")
        .and_then(|value| value.to_str().ok())
        .context("missing X-YAM-Token")?;
    if app.runtime_tokens.lock().await.get(id).map(String::as_str) != Some(token) {
        return Err(ApiError(anyhow!("invalid runtime token")));
    }
    let manifest = app.plugins.lock().await.manifest(id)?;
    if !manifest.permissions.iter().any(|item| item == permission) {
        return Err(ApiError(anyhow!("package lacks {permission} permission")));
    }
    Ok(())
}
async fn runtime_device(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<DeviceProfile>, ApiError> {
    runtime_permission(&a, &id, &headers, "exec").await?;
    Ok(Json(a.device.lock().await.clone()))
}
async fn runtime_sensors(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<SensorInfo>>, ApiError> {
    runtime_permission(&a, &id, &headers, "sensors").await?;
    Ok(get_sensors().await)
}
async fn runtime_led(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(frame): Json<Frame>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.draw(frame.colors).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn runtime_led_off(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "led").await?;
    a.led.off().await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn runtime_clock(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<ClockRequest>,
) -> Result<StatusCode, ApiError> {
    runtime_permission(&a, &id, &headers, "clock").await?;
    put_clock(Json(request)).await
}
async fn runtime_volume(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<VolumeInfo>, ApiError> {
    runtime_permission(&a, &id, &headers, "volume").await?;
    Ok(Json(volume_info()?))
}
async fn runtime_volume_set(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<VolumeRequest>,
) -> Result<Json<VolumeInfo>, ApiError> {
    runtime_permission(&a, &id, &headers, "volume").await?;
    Ok(Json(set_volume(request.percent)?))
}
async fn runtime_system(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<SystemInfo>, ApiError> {
    runtime_permission(&a, &id, &headers, "system").await?;
    Ok(Json(SystemInfo {
        hostname: fs::read_to_string("/etc/hostname")?.trim().to_string(),
        uptime: fs::read_to_string("/proc/uptime")?,
        meminfo: fs::read_to_string("/proc/meminfo")?,
    }))
}
async fn runtime_bluetooth(
    State(a): State<App>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    runtime_permission(&a, &id, &headers, "bluetooth").await?;
    Ok(command_output("bluetoothctl", &["show".into()])?)
}
async fn runtime_bluetooth_action(
    State(a): State<App>,
    AxumPath((id, action)): AxumPath<(String, String)>,
    headers: HeaderMap,
    request: Option<Json<BluetoothRequest>>,
) -> Result<String, ApiError> {
    runtime_permission(&a, &id, &headers, "bluetooth").await?;
    let request = request.map(|value| value.0).unwrap_or(BluetoothRequest {
        on: None,
        mac: None,
    });
    let args = match action.as_str() {
        "power" => vec![
            "power".into(),
            if request.on.unwrap_or(false) {
                "on".into()
            } else {
                "off".into()
            },
        ],
        "scan" => vec![
            "scan".into(),
            if request.on.unwrap_or(false) {
                "on".into()
            } else {
                "off".into()
            },
        ],
        action @ ("pair" | "connect" | "disconnect" | "remove") => {
            let mac = request.mac.context("Bluetooth action requires mac")?;
            if !valid_mac(&mac) {
                return Err(ApiError(anyhow!("invalid Bluetooth MAC")));
            }
            vec![action.into(), mac]
        }
        _ => return Err(ApiError(anyhow!("unknown Bluetooth action"))),
    };
    Ok(command_output("bluetoothctl", &args)?)
}
async fn get_settings(State(a): State<App>) -> Json<Settings> {
    Json(a.settings.lock().await.clone())
}
async fn get_device(State(a): State<App>) -> Json<DeviceProfile> {
    Json(a.device.lock().await.clone())
}
async fn get_sensors() -> Json<Vec<SensorInfo>> {
    Json(
        discover_sensors()
            .into_iter()
            .map(|sensor| SensorInfo {
                value: sensor_value(&sensor).ok(),
                id: sensor.id,
                kind: sensor.kind,
                unit: sensor.unit,
            })
            .collect(),
    )
}
async fn get_clock() -> Json<ClockInfo> {
    Json(ClockInfo {
        available: clock_available(),
        segments: clock_segments(),
    })
}
async fn put_clock(Json(request): Json<ClockRequest>) -> Result<StatusCode, ApiError> {
    if request.off {
        clock_release()?;
    } else if let Some(time) = request.time {
        clock_time(&time, request.brightness.unwrap_or(170))?;
    } else if let Some(segments) = request.segments {
        validate_clock_segments(&segments)?;
        clock_write(&segments)?;
    } else {
        return Err(ApiError(anyhow!("pass time, segments or off=true")));
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn put_device(
    State(a): State<App>,
    Json(d): Json<DeviceProfile>,
) -> Result<StatusCode, ApiError> {
    validate_id(&d.model)?;
    write_atomic(
        &a.root.join("device.toml"),
        toml::to_string_pretty(&d)?.as_bytes(),
    )?;
    *a.device.lock().await = d;
    Ok(StatusCode::NO_CONTENT)
}
async fn put_settings(
    State(a): State<App>,
    Json(s): Json<Settings>,
) -> Result<StatusCode, ApiError> {
    if s.led.count == 0 || s.led.count > 128 {
        return Err(ApiError(anyhow!("LED count must be 1..128")));
    };
    if s.web.ui && !s.web.api {
        return Err(ApiError(anyhow!("web.ui requires web.api")));
    }
    write_atomic(
        &a.root.join("settings.toml"),
        toml::to_string_pretty(&s)?.as_bytes(),
    )?;
    *a.settings.lock().await = s.clone();
    *a.led.settings.lock().await = s.led;
    Ok(StatusCode::NO_CONTENT)
}
async fn led_set(State(a): State<App>, Json(f): Json<Frame>) -> Result<StatusCode, ApiError> {
    a.led.draw(f.colors).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn led_off(State(a): State<App>) -> Result<StatusCode, ApiError> {
    a.led.off().await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Debug)]
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}

fn shell_quote(value: &std::ffi::OsStr) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn stage_release_update(executable: &Path, data_dir: &Path, listen: &str) -> Result<bool> {
    const RELEASES: &str = "https://github.com/nyakokitsu/yam/releases/latest/download";
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let sums = client
        .get(format!("{RELEASES}/SHA256SUMS"))
        .header("User-Agent", "yam-auto-updater")
        .send()?
        .error_for_status()?
        .text()?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next()) {
                (Some(hash), Some("yamd-aarch64-linux-musl")) => Some(hash),
                _ => None,
            }
        })
        .context("release SHA256SUMS has no yamd checksum")?;
    if format!(
        "{:x}",
        Sha256::digest(fs::read(executable).unwrap_or_default())
    ) == expected
    {
        return Ok(false);
    }
    let binary = client
        .get(format!("{RELEASES}/yamd-aarch64-linux-musl"))
        .header("User-Agent", "yam-auto-updater")
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    if format!("{:x}", Sha256::digest(&binary)) != expected {
        bail!("release checksum mismatch")
    }
    let staged = executable.with_extension("yam-update");
    fs::write(&staged, binary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    let helper = data_dir.join(".yam-restart-update.sh");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nsleep 1\nmv {} {}\nexec {} --listen {} --data-dir {}\n",
            shell_quote(staged.as_os_str()),
            shell_quote(executable.as_os_str()),
            shell_quote(executable.as_os_str()),
            shell_quote(std::ffi::OsStr::new(listen)),
            shell_quote(data_dir.as_os_str()),
        ),
    )?;
    Command::new("sh").arg(helper).spawn()?;
    Ok(true)
}

/// Binary packages reach the daemon on loopback unless it listens on one address only.
fn runtime_api_url(listen: &str) -> Result<String> {
    let mut address: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("listen address {listen} must be IP:port"))?;
    if address.ip().is_unspecified() {
        address.set_ip(match address {
            std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    Ok(format!("http://{address}"))
}
fn start_auto_updater(app: App, executable: PathBuf, listen: String) {
    task::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if app.settings.lock().await.updates.enabled {
                let executable = executable.clone();
                let data_dir = app.root.clone();
                let listen = listen.clone();
                match task::spawn_blocking(move || {
                    stage_release_update(&executable, &data_dir, &listen)
                })
                .await
                {
                    Ok(Ok(true)) => {
                        tracing::info!("release update staged; restarting yamd");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        std::process::exit(0);
                    }
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "automatic update check failed"),
                    Err(error) => tracing::warn!(%error, "automatic update task failed"),
                }
            }
            let interval = app.settings.lock().await.updates.interval_minutes;
            tokio::time::sleep(Duration::from_secs(interval.clamp(5, 24 * 60) * 60)).await;
        }
    });
}
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    fs::create_dir_all(&args.data_dir)?;
    let settings_path = args.data_dir.join("settings.toml");
    // A broken settings.toml must not silently fall back to defaults: they
    // re-enable the web API and auto-updates.
    let settings: Settings = match fs::read_to_string(&settings_path) {
        Ok(raw) => {
            toml::from_str(&raw).with_context(|| format!("invalid {}", settings_path.display()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut settings = Settings::default();
            if let Some((profile, count)) = detect_led_profile(&settings.led.sysfs_root) {
                settings.led.profile = profile.into();
                settings.led.count = count;
            }
            settings
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", settings_path.display()))
        }
    };
    let led = Arc::new(Led {
        settings: Arc::new(Mutex::new(settings.led.clone())),
    });
    let mut device = detect_device(&args.data_dir, args.model);
    let led_features = if configured_rgb_ring(&settings.led) {
        vec![
            "led.ring.rgb".into(),
            format!("led.ring.{}", settings.led.count),
            "led.pattern.v1".into(),
        ]
    } else if Path::new(&settings.led.sysfs_root).exists() {
        vec!["led.sysfs".into()]
    } else {
        vec![]
    };
    for feature in led_features {
        if !device.features.contains(&feature) {
            device.features.push(feature);
        }
    }
    tracing::info!(model=%device.model,features=?device.features,"detected device");
    let app = App {
        root: args.data_dir.clone(),
        settings: Arc::new(Mutex::new(settings)),
        device: Arc::new(Mutex::new(device)),
        led,
        plugins: Arc::new(Mutex::new(PluginManager::new(
            args.data_dir.join("plugins"),
        )?)),
        runtime_tokens: Arc::new(Mutex::new(HashMap::new())),
        runtime_api_url: runtime_api_url(&args.listen)?,
    };
    start_auto_updater(app.clone(), std::env::current_exe()?, args.listen.clone());
    let services = {
        let device = app.device.lock().await.clone();
        app.plugins
            .lock()
            .await
            .list(&device)?
            .into_iter()
            .filter(|package| package.enabled && package.service_enabled)
            .map(|package| package.manifest.id)
            .collect::<Vec<_>>()
    };
    for id in services {
        if let Err(error) = start_package(app.clone(), id.clone(), Vec::new()).await {
            tracing::error!(package = %id, %error, "service package failed to start");
        }
    }
    let web = app.settings.lock().await.web.clone();
    if web.ui && !web.api {
        bail!("web.ui requires web.api")
    }
    if !web.ui && !web.api {
        tracing::info!("web interface is disabled");
        std::future::pending::<()>().await;
    }
    let mut router = Router::new();
    if web.ui {
        router = router.route("/", get(index));
    }
    if web.api {
        router = router
            .route("/api/plugins", get(list).post(install))
            .route("/api/plugins/registry", post(install_registry))
            .route("/api/plugins/:id", delete(remove))
            .route("/api/plugins/:id/toggle", post(toggle))
            .route("/api/plugins/:id/invoke", post(invoke))
            .route("/api/plugins/:id/service/enable", post(enable_service))
            .route("/api/plugins/:id/service/disable", post(disable_service))
            .route("/api/plugins/:id/service/start", post(start_service))
            .route("/api/plugins/:id/stop", post(stop_plugin))
            .route("/api/plugins/:id/logs", get(package_logs))
            .route("/api/runtime/:id/device", get(runtime_device))
            .route("/api/runtime/:id/sensors", get(runtime_sensors))
            .route("/api/runtime/:id/led", post(runtime_led))
            .route("/api/runtime/:id/led/off", post(runtime_led_off))
            .route("/api/runtime/:id/clock", post(runtime_clock))
            .route(
                "/api/runtime/:id/volume",
                get(runtime_volume).put(runtime_volume_set),
            )
            .route("/api/runtime/:id/system", get(runtime_system))
            .route("/api/runtime/:id/bluetooth", get(runtime_bluetooth))
            .route(
                "/api/runtime/:id/bluetooth/:action",
                post(runtime_bluetooth_action),
            )
            .route("/api/settings", get(get_settings).put(put_settings))
            .route("/api/device", get(get_device).put(put_device))
            .route("/api/sensors", get(get_sensors))
            .route("/api/clock", get(get_clock).put(put_clock))
            .route("/api/led", post(led_set))
            .route("/api/led/off", post(led_off));
    }
    let router = router
        .layer(DefaultBodyLimit::max(67 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(app);
    let l = TcpListener::bind(&args.listen).await?;
    tracing::info!("web UI: http://{}", args.listen);
    axum::serve(l, router).await?;
    Ok(())
}
const UI: &str = include_str!("../web/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    struct PlumFixture {
        root: PathBuf,
        settings: LedSettings,
    }
    impl PlumFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("yam-plum-led-{}", uuid::Uuid::new_v4()));
            let settings = LedSettings {
                profile: "plum".into(),
                count: 3,
                sysfs_root: root.to_string_lossy().into_owned(),
                path_template: None,
            };
            for i in 0..3 {
                for channel in ["red", "green", "blue"] {
                    let p = led_brightness_path(&settings, channel, i).unwrap();
                    fs::create_dir_all(p.parent().unwrap()).unwrap();
                    fs::write(&p, "73").unwrap();
                    fs::write(p.with_file_name("trigger"), "[none] timer led_ring_fb").unwrap();
                }
            }
            let clock = root.join("plum:white:clockA0");
            fs::create_dir_all(&clock).unwrap();
            fs::write(clock.join("brightness"), "19").unwrap();
            fs::write(clock.join("trigger"), "[none] timer led_ring_fb").unwrap();
            Self { root, settings }
        }
        fn led(&self) -> Led {
            Led {
                settings: Arc::new(Mutex::new(self.settings.clone())),
            }
        }
        fn assert_untouched(&self) {
            for i in 0..3 {
                for channel in ["red", "green", "blue"] {
                    assert_eq!(
                        fs::read_to_string(
                            led_brightness_path(&self.settings, channel, i).unwrap()
                        )
                        .unwrap(),
                        "73"
                    );
                }
            }
            assert_eq!(
                fs::read_to_string(self.root.join("plum:white:clockA0/brightness")).unwrap(),
                "19"
            );
            assert_eq!(
                fs::read_to_string(self.root.join("plum:white:clockA0/trigger")).unwrap(),
                "[none] timer led_ring_fb"
            );
        }
    }
    impl Drop for PlumFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn plum_off_returns_native_rgb_without_clearing_clock_or_frame() {
        let f = PlumFixture::new();
        let led = f.led();
        led.off().await.unwrap();
        // Repeated release also accepts an already-selected/native trigger.
        led.off().await.unwrap();
        for i in 0..3 {
            for channel in ["red", "green", "blue"] {
                let p = led_brightness_path(&f.settings, channel, i)
                    .unwrap()
                    .with_file_name("trigger");
                assert_eq!(fs::read_to_string(p).unwrap(), "led_ring_fb");
            }
        }
        f.assert_untouched();
    }

    #[tokio::test]
    async fn plum_release_checks_all_channels_before_writing() {
        let f = PlumFixture::new();
        let last = led_brightness_path(&f.settings, "blue", 2)
            .unwrap()
            .with_file_name("trigger");
        fs::write(&last, "[none] timer").unwrap();
        assert!(f
            .led()
            .off()
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
        let first = led_brightness_path(&f.settings, "red", 0)
            .unwrap()
            .with_file_name("trigger");
        assert_eq!(
            fs::read_to_string(first).unwrap(),
            "[none] timer led_ring_fb"
        );
        f.assert_untouched();
    }

    #[tokio::test]
    async fn plum_release_rejects_missing_trigger_without_writes() {
        let f = PlumFixture::new();
        fs::remove_file(
            led_brightness_path(&f.settings, "blue", 2)
                .unwrap()
                .with_file_name("trigger"),
        )
        .unwrap();
        assert!(f.led().off().await.is_err());
        assert_eq!(
            fs::read_to_string(
                led_brightness_path(&f.settings, "red", 0)
                    .unwrap()
                    .with_file_name("trigger")
            )
            .unwrap(),
            "[none] timer led_ring_fb"
        );
        f.assert_untouched();
    }

    #[test]
    fn plum_release_rejects_unknown_layout() {
        let mut f = PlumFixture::new();
        let led = f.led();
        f.settings.count = 6;
        assert!(led.release_plum(&f.settings).is_err());
        f.settings.count = 3;
        f.settings.profile = "custom".into();
        assert!(led.release_plum(&f.settings).is_err());
    }

    #[test]
    fn ipc_decodes_speech_and_alarm() {
        let speech = [0x1a, 0x04, 0x12, 0x02, b'h', b'i'];
        assert_eq!(
            decode_ipc_event("aliced", &speech).unwrap().unwrap()["transcript"],
            "hi"
        );
        let alarm = [
            0xba, 0x0d, 0x08, 0x0a, 0x06, 0x0a, 0x02, b'a', b'1', 0x10, 0x02,
        ];
        assert_eq!(
            decode_ipc_event("alarmd", &alarm).unwrap().unwrap()["id"],
            "a1"
        );
    }

    #[test]
    fn ipc_frame_length_uses_signed_10_mib_bound() {
        assert_eq!(
            ipc_frame_length((IPC_MAX_FRAME_SIZE as i32).to_le_bytes()).unwrap(),
            IPC_MAX_FRAME_SIZE
        );
        assert!(ipc_frame_length((-1i32).to_le_bytes()).is_err());
        assert!(ipc_frame_length(((IPC_MAX_FRAME_SIZE + 1) as i32).to_le_bytes()).is_err());
    }

    #[test]
    fn alice_cancel_uses_remoting_envelope() {
        let mut method = Vec::new();
        push_varint(&mut method, 1 << 3);
        push_varint(&mut method, 3);
        let mut remoting = Vec::new();
        push_bytes(&mut remoting, 7, b"AliceCapability");
        push_bytes(&mut remoting, 8, &method);
        let mut message = Vec::new();
        push_bytes(&mut message, 252, &remoting);
        push_varint(&mut message, 253 << 3);
        push_varint(&mut message, 210);
        assert_eq!(varint_field(&message, 253).unwrap(), Some(210));
        let remoting = nested_field(&message, 252).unwrap().unwrap();
        assert_eq!(
            text_field(&remoting, 7).unwrap().as_deref(),
            Some("AliceCapability")
        );
        let method = nested_field(&remoting, 8).unwrap().unwrap();
        assert_eq!(varint_field(&method, 1).unwrap(), Some(3));
    }

    #[test]
    fn voice_command_matches_alice_prefix_and_normalizes_text() {
        assert!(command_matches("ананас", "Алиса, ананас!"));
        assert!(command_matches("ананас", "ананас"));
        assert!(!command_matches("ананас", "алиса банан"));
    }

    #[test]
    fn model_names_are_normalized_for_package_compatibility() {
        assert_eq!(canonical_model("Yandex Micro"), "yandexmicro");
        assert_eq!(canonical_model("yandex,micro"), "yandexmicro");
        assert_eq!(canonical_model("yandex,station-mini2"), "yandexmini_2");
    }

    #[test]
    fn availability_reports_device_capabilities_but_not_exec() {
        let device = DeviceProfile {
            model: "yandexmicro".into(),
            architecture: "aarch64".into(),
            features: vec!["led.ring.rgb".into(), "audio.play.aplay".into()],
            source: "test".into(),
        };
        assert!(lua_api_available(&device, "led").unwrap());
        assert!(lua_api_available(&device, "audio").unwrap());
        assert!(!lua_api_available(&device, "bluetooth").unwrap());
        assert!(lua_api_available(&device, "exec").is_err());
    }

    #[test]
    fn network_policy_blocks_loopback() {
        let policy = NetworkPolicy {
            allow: vec!["*".into()],
            allow_insecure_http: true,
        };
        assert!(network_url_allowed(&policy, "http://127.0.0.1/").is_err());
    }

    #[test]
    fn input_events_decode_for_both_linux_word_sizes() {
        let mut event64 = vec![];
        event64.extend(10i64.to_ne_bytes());
        event64.extend(2_000i64.to_ne_bytes());
        event64.extend(1u16.to_ne_bytes());
        event64.extend(2u16.to_ne_bytes());
        event64.extend((-1i32).to_ne_bytes());
        assert_eq!(decode_input_event("mute", &event64, 8).unwrap().value, -1);

        let mut event32 = vec![];
        event32.extend(10i32.to_ne_bytes());
        event32.extend(2_000i32.to_ne_bytes());
        event32.extend(3u16.to_ne_bytes());
        event32.extend(4u16.to_ne_bytes());
        event32.extend(5i32.to_ne_bytes());
        assert_eq!(
            decode_input_event("mute", &event32, 4).unwrap().time_ms,
            10_002
        );
        assert!(decode_input_event("mute", &event32[..15], 4).is_err());
    }

    #[test]
    fn input_scope_is_limited_to_declared_devices() {
        let manifest: Manifest = toml::from_str(
            "id='p'\nname='p'\nversion='1'\nentry='main.lua'\npermissions=['input']\n[input]\ndevices=['mute']\n",
        )
        .unwrap();
        assert!(validate_input_policy(&manifest).is_ok());
        let devices = resolve_input_devices(
            "N: Name=\"mute\"\nH: Handlers=event2\n\nN: Name=\"other\"\nH: Handlers=event3\n",
            input_scope(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            devices,
            vec![("mute".into(), PathBuf::from("/dev/input/event2"))]
        );
    }

    #[test]
    fn runtime_helpers_parse_native_values() {
        let output = "Simple mixer control 'Master',0\n  Front Left: Playback 42 [42%] [on]\n";
        assert_eq!(volume_percent(output), Some(42));
        assert!(valid_mac("AA:BB:CC:DD:EE:FF"));
        assert!(!valid_mac("not-a-mac"));
    }
}
