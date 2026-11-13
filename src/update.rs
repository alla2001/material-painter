//! Updating the app from its GitHub releases: find the latest release, download the
//! package made for this platform and install (the setup program on Windows installs, the
//! files of an unpacked zip or tarball, the macOS app bundle).
//!
//! Only signed packages install: the release workflow signs each one with a private
//! Ed25519 key (tools/update-sign, the UPDATE_SIGNING_KEY secret) and uploads the signature
//! as `<package>.sig`; the app checks it against `SIGNING_KEY`. So a package that didn't
//! come from the release workflow is refused, even from the right GitHub page.
//!
//! The new files go in once the window has closed: `apply_on_exit` keeps the prepared
//! update and `finish` (called by `main` after the event loop returns) swaps it in and,
//! if asked, starts the new version.

use serde_json::Value;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const LATEST_URL: &str = "https://api.github.com/repos/alla2001/material-painter/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/alla2001/material-painter/releases";

/// The public half of the key the release workflow signs packages with (made by
/// `update-sign keygen`; the private half is the UPDATE_SIGNING_KEY secret).
const SIGNING_KEY: [u8; 32] = hex32("0e4df04e2267f122e0890f1389db8ea5ca7961e49489b09444f39ad387216b48");

const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not lowercase hex"),
        }
    }
    let (b, mut out, mut i) = (s.as_bytes(), [0u8; 32], 0);
    assert!(b.len() == 64);
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// What a package's signature signs (the same in tools/update-sign).
fn signed_message(name: &str, sha256_hex: &str) -> String {
    format!("material-painter update
{name}
{sha256_hex}")
}

/// Whether `signature` (hex, from the `.sig` file) signs the package `name` with that
/// digest under `key`.
fn signature_ok(key: &[u8], name: &str, sha256_hex: &str, signature: &str) -> bool {
    let sig = signature.trim();
    let bytes: Option<Vec<u8>> = (sig.len() % 2 == 0)
        .then(|| (0..sig.len()).step_by(2).map(|i| sig.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect())
        .flatten();
    bytes.is_some_and(|sig| ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key).verify(signed_message(name, sha256_hex).as_bytes(), &sig).is_ok())
}

/// What a cancelled download reports.
pub const CANCELLED: &str = "Download cancelled.";
/// Why a package without a signature doesn't install.
pub const UNSIGNED: &str = "This release isn't signed for installing from the app.";

/// The program file in a release package.
const EXE_NAME: &str = if cfg!(windows) { "MaterialPainter.exe" } else { "material-painter" };
/// Next to an unpacked install: the new files while they wait, and the replaced ones until
/// the next start.
const STAGE_DIR: &str = ".update";
const OLD_DIR: &str = ".old-version";

/// A release version, major.minor.patch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// "v0.1.7", "0.2" or "1.0.0-beta" (the suffix is ignored).
    pub fn parse(s: &str) -> Option<Version> {
        let core = s.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
        let mut parts = core.split('.').map(|p| p.parse::<u32>().ok());
        let major = parts.next()??;
        let minor = parts.next().unwrap_or(Some(0))?;
        let patch = parts.next().unwrap_or(Some(0))?;
        parts.next().is_none().then_some(Version(major, minor, patch))
    }

    /// This build's version.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is x.y.z")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    /// The release notes (Markdown).
    pub notes: String,
    /// The release's web page.
    pub page: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex, when GitHub lists one.
    pub sha256: Option<String>,
    /// Where its `.sig` is, if the release has one.
    pub signature_url: Option<String>,
}

/// Reads a release from GitHub's API answer.
pub fn parse_release(v: &Value) -> Result<Release, String> {
    let tag = v["tag_name"].as_str().ok_or("The release has no version tag.")?;
    let version = Version::parse(tag).ok_or_else(|| format!("The release tag {tag} isn't a version."))?;
    let assets = v["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a["name"].as_str()?.to_string(),
                        url: a["browser_download_url"].as_str()?.to_string(),
                        size: a["size"].as_u64().unwrap_or(0),
                        sha256: a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase),
                        signature_url: None,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let assets = assets
        .iter()
        .filter(|a| !a.name.ends_with(".sig"))
        .map(|a| {
            let sig = format!("{}.sig", a.name);
            Asset { signature_url: assets.iter().find(|s| s.name == sig).map(|s| s.url.clone()), ..a.clone() }
        })
        .collect();
    Ok(Release {
        version,
        notes: v["body"].as_str().unwrap_or_default().trim().to_string(),
        page: v["html_url"].as_str().unwrap_or(RELEASES_PAGE).to_string(),
        assets,
    })
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(crate::library::USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// The newest published release (drafts and pre-releases aren't listed).
pub fn latest(agent: &ureq::Agent) -> Result<Release, String> {
    let mut resp = agent.get(LATEST_URL).header("Accept", "application/vnd.github+json").call().map_err(|e| match e {
        ureq::Error::StatusCode(403 | 429) => "GitHub is limiting requests from this network. Try again in an hour.".to_string(),
        ureq::Error::StatusCode(404) => "No release is published yet.".to_string(),
        e => format!("Couldn't reach GitHub: {e}"),
    })?;
    let text = resp.body_mut().with_config().limit(8 << 20).read_to_string().map_err(|e| format!("Couldn't read GitHub's answer: {e}"))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("Couldn't read GitHub's answer: {e}"))?;
    parse_release(&json)
}

// ---------------------------------------------------------------- how this copy was installed

/// How this copy was installed, which decides what to download and how it goes in.
#[derive(Clone, Debug, PartialEq)]
pub enum Install {
    /// Windows, by the setup program (an uninstaller sits next to the program): run the
    /// new setup.
    WindowsSetup { dir: PathBuf },
    /// A folder unpacked from the Windows zip or the Linux tarball: replace its files.
    Folder { dir: PathBuf },
    /// The macOS app bundle from the .dmg: replace the bundle.
    MacBundle { app: PathBuf },
}

impl Install {
    /// Finds out from where the running program is. An error says why this copy can't
    /// update itself.
    pub fn detect() -> Result<Install, String> {
        let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the program file: {e}"))?;
        Install::detect_at(&exe)
    }

    /// `detect` for the running program, worked out once.
    pub fn this() -> &'static Result<Install, String> {
        static THIS: std::sync::OnceLock<Result<Install, String>> = std::sync::OnceLock::new();
        THIS.get_or_init(Install::detect)
    }

    fn detect_at(exe: &Path) -> Result<Install, String> {
        let dir = exe.parent().ok_or("Couldn't find the program's folder.")?;
        let dev = exe.ancestors().any(|a| a.file_name().is_some_and(|n| n == "target") && a.parent().is_some_and(|p| p.join("Cargo.toml").is_file()));
        if dev {
            return Err("This is a build from the source code. Update it with git and cargo.".into());
        }
        if cfg!(target_os = "macos") {
            // …/Material Painter.app/Contents/MacOS/MaterialPainter
            let app = dir
                .parent()
                .and_then(Path::parent)
                .filter(|app| dir.file_name().is_some_and(|n| n == "MacOS") && app.extension().is_some_and(|e| e == "app"));
            return app.map(|app| Install::MacBundle { app: app.to_path_buf() }).ok_or_else(|| "Only the app from the .dmg can update itself.".into());
        }
        if cfg!(windows) && dir.join("unins000.exe").is_file() {
            return Ok(Install::WindowsSetup { dir: dir.to_path_buf() });
        }
        if dir.join("assets").is_dir() {
            return Ok(Install::Folder { dir: dir.to_path_buf() });
        }
        Err("This copy isn't laid out like a release download, so it can't update itself.".into())
    }

    /// How the release package for this install ends.
    fn asset_suffix(&self) -> Option<&'static str> {
        let x64 = cfg!(target_arch = "x86_64");
        match self {
            Install::WindowsSetup { .. } if x64 => Some("-setup.exe"),
            Install::Folder { .. } if x64 && cfg!(windows) => Some("-windows-x64.zip"),
            Install::Folder { .. } if x64 && cfg!(target_os = "linux") => Some("-linux-x64.tar.gz"),
            Install::MacBundle { .. } => Some("-macos-universal.dmg"),
            _ => None,
        }
    }

    /// The package in `release` for this install. Its name carries the release's version,
    /// which the signature covers, so an old package can't pass for a new release.
    pub fn asset<'a>(&self, release: &'a Release) -> Option<&'a Asset> {
        let name = format!("MaterialPainter-{}{}", release.version, self.asset_suffix()?);
        release.assets.iter().find(|a| a.name == name)
    }

    /// Whether installing runs the setup program (which may ask for permission).
    pub fn uses_setup(&self) -> bool {
        matches!(self, Install::WindowsSetup { .. })
    }
}

// ---------------------------------------------------------------- download

fn downloads_dir() -> Option<PathBuf> {
    crate::crash::data_dir().map(|d| d.join("updates"))
}

/// Downloads a release package into the app's data folder, checking its size, digest and
/// signature. `progress` hears bytes done and total; setting `cancel` stops it.
pub fn download(agent: &ureq::Agent, asset: &Asset, cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<PathBuf, String> {
    let sig_url = asset.signature_url.as_deref().ok_or(UNSIGNED)?;
    let signature = agent
        .get(sig_url)
        .call()
        .and_then(|mut r| r.body_mut().with_config().limit(4096).read_to_string())
        .map_err(|e| format!("Couldn't download the update's signature: {e}"))?;
    let verify = |digest: &str| {
        if signature_ok(&SIGNING_KEY, &asset.name, digest, &signature) {
            Ok(())
        } else {
            Err("The download isn't signed with Material Painter's release key, so it wasn't installed.".to_string())
        }
    };
    fetch_package(agent, asset, &verify, cancel, progress)
}

/// Downloads a package, checking its size and GitHub's digest, then `verify` with its
/// SHA-256 (hex). Only a package that passes keeps its name; anything else is deleted.
fn fetch_package(agent: &ureq::Agent, asset: &Asset, verify: &dyn Fn(&str) -> Result<(), String>, cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<PathBuf, String> {
    if asset.name.contains(['/', '\\']) || asset.name.starts_with('.') {
        return Err(format!("The package name {} isn't a file name.", asset.name));
    }
    let dir = downloads_dir().ok_or("There's no folder to download into.")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&asset.name);
    let part = dir.join(format!("{}.part", asset.name));
    let result = (|| {
        let mut resp = agent.get(&asset.url).call().map_err(|e| format!("Couldn't download the update: {e}"))?;
        let total = resp.body().content_length().unwrap_or(asset.size);
        let mut reader = resp.body_mut().with_config().limit(2 << 30).reader();
        let mut file = std::io::BufWriter::new(std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?);
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let (mut done, mut buf) = (0u64, vec![0u8; 256 * 1024]);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            let n = reader.read(&mut buf).map_err(|e| format!("The download broke off: {e}"))?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", part.display()))?;
            done += n as u64;
            progress(done, total);
        }
        file.flush().map_err(|e| format!("{}: {e}", part.display()))?;
        if asset.size != 0 && done != asset.size {
            return Err("The download is incomplete. Try again.".to_string());
        }
        let got: String = hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if asset.sha256.as_ref().is_some_and(|want| &got != want) {
            return Err("The download doesn't match the release's checksum. Try again.".to_string());
        }
        verify(&got)
    })();
    match result {
        Ok(()) => match std::fs::rename(&part, &path) {
            Ok(()) => Ok(path),
            Err(e) => Err(format!("{}: {e}", path.display())),
        },
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------- preparing and installing

/// A downloaded update, ready to go in when the app closes.
#[derive(Debug)]
pub enum Pending {
    RunSetup { setup: PathBuf, dir: PathBuf },
    /// The new files, unpacked into `staged` inside the install folder.
    SwapFolder { dir: PathBuf, staged: PathBuf },
    /// The new bundle, copied next to the current one.
    SwapBundle { app: PathBuf, staged: PathBuf },
}

/// Unpacks a downloaded package next to the install, so swapping it in when the app
/// closes is only renames.
pub fn prepare(install: &Install, package: &Path) -> Result<Pending, String> {
    match install {
        Install::WindowsSetup { dir } => Ok(Pending::RunSetup { setup: package.to_path_buf(), dir: dir.clone() }),
        Install::Folder { dir } => {
            let staged = dir.join(STAGE_DIR);
            let _ = std::fs::remove_dir_all(&staged);
            std::fs::create_dir(&staged)
                .map_err(|e| format!("Can't write to {}: {e}. Download the update from the release page instead.", dir.display()))?;
            let unpacked = unpack(package, &staged).and_then(|()| {
                if staged.join(EXE_NAME).is_file() { Ok(()) } else { Err("The downloaded package has no program in it.".to_string()) }
            });
            match unpacked {
                Ok(()) => Ok(Pending::SwapFolder { dir: dir.clone(), staged }),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&staged);
                    Err(e)
                }
            }
        }
        Install::MacBundle { app } => prepare_bundle(app, package),
    }
}

/// A path inside an archive made safe to join to a folder, without the package's top
/// folder ("Material Painter/assets/x.png" → "assets/x.png"). `None` for the top folder
/// itself and for names that would land outside.
fn inner_path(name: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect();
    // A drive ("C:") or stream name never belongs in a package path.
    if parts.iter().any(|p| p.contains(':')) {
        return None;
    }
    let rest = parts.get(1..).filter(|r| !r.is_empty())?;
    let path: PathBuf = rest.iter().collect();
    path.components().all(|c| matches!(c, Component::Normal(_))).then_some(path)
}

fn unpack(package: &Path, out: &Path) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("Couldn't unpack {}: {e}", package.display());
    let name = package.to_string_lossy();
    if name.ends_with(".zip") {
        let file = std::fs::File::open(package).map_err(|e| err(&e))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| err(&e))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| err(&e))?;
            let Some(rel) = inner_path(entry.name()) else { continue };
            let dest = out.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&dest).map_err(|e| err(&e))?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| err(&e))?;
            }
            let mut f = std::fs::File::create(&dest).map_err(|e| err(&e))?;
            std::io::copy(&mut entry, &mut f).map_err(|e| err(&e))?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode & 0o777));
            }
        }
        return Ok(());
    }
    if name.ends_with(".tar.gz") {
        return untar(package, out).map_err(|e| err(&e));
    }
    Err(err(&"not a zip or tarball"))
}

#[cfg(target_os = "linux")]
fn untar(package: &Path, out: &Path) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(package)?));
    archive.set_preserve_permissions(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let Some(rel) = inner_path(&name) else { continue };
        let dest = out.join(rel);
        match entry.header().entry_type() {
            tar::EntryType::Directory => std::fs::create_dir_all(&dest)?,
            tar::EntryType::Regular => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                entry.unpack(&dest)?;
            }
            // Links and the like aren't in our packages.
            _ => {}
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn untar(_package: &Path, _out: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("tarballs are the Linux package"))
}

/// Copies the app out of the downloaded .dmg next to the current bundle.
fn prepare_bundle(app: &Path, dmg: &Path) -> Result<Pending, String> {
    let parent = app.parent().ok_or("Couldn't find the folder the app is in.")?;
    let name = app.file_stem().map_or_else(|| "Material Painter".into(), |n| n.to_string_lossy().into_owned());
    let staged = parent.join(format!(".{name}.update"));
    let _ = std::fs::remove_dir_all(&staged);
    let mount = downloads_dir().ok_or("There's no folder to open the download in.")?.join("mount");
    std::fs::create_dir_all(&mount).map_err(|e| format!("{}: {e}", mount.display()))?;
    run("hdiutil", &["attach".as_ref(), "-nobrowse".as_ref(), "-readonly".as_ref(), "-noautoopen".as_ref(), "-mountpoint".as_ref(), mount.as_os_str(), dmg.as_os_str()])?;
    let copied = std::fs::read_dir(&mount)
        .map_err(|e| format!("{}: {e}", mount.display()))
        .and_then(|list| {
            list.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "app")).ok_or_else(|| "The download has no app in it.".to_string())
        })
        .and_then(|src| {
            run("ditto", &[src.as_os_str(), staged.as_os_str()])
                .map_err(|e| format!("{e} Can't write next to {}? Download the update from the release page instead.", app.display()))
        });
    let _ = run("hdiutil", &["detach".as_ref(), mount.as_os_str(), "-quiet".as_ref()]);
    match copied {
        Ok(()) if staged.join("Contents/MacOS").is_dir() => Ok(Pending::SwapBundle { app: app.to_path_buf(), staged }),
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&staged);
            Err("The app in the download is incomplete.".into())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged);
            Err(e)
        }
    }
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let out = Command::new(program).args(args).output().map_err(|e| format!("Couldn't run {program}: {e}."))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed: {}.", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static RELAUNCH: AtomicBool = AtomicBool::new(false);

/// Installs `update` when the app closes.
pub fn apply_on_exit(update: Pending) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(update);
}

/// Whether the new version starts once it's in.
pub fn set_relaunch(on: bool) {
    RELAUNCH.store(on, Ordering::Relaxed);
}

/// Swaps in a prepared update (the window has closed). Problems show in a message box,
/// since there's no window left to show them in.
pub fn finish() {
    let Some(update) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    if let Err(e) = install(update, RELAUNCH.load(Ordering::Relaxed)) {
        rfd::MessageDialog::new()
            .set_title("Material Painter update")
            .set_level(rfd::MessageLevel::Error)
            .set_description(format!("The update couldn't be installed: {e}\n\nYou can download it from {RELEASES_PAGE}."))
            .show();
    }
}

fn install(update: Pending, relaunch: bool) -> Result<(), String> {
    match update {
        Pending::RunSetup { setup, dir } => {
            // Inno Setup: no wizard, just its progress window. It waits for this process to
            // end (WAITPID, see installer/material-painter.iss), keeps the install folder
            // and install mode, and starts the app again if asked.
            let mut cmd = Command::new(&setup);
            cmd.args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/NOCANCEL"]);
            let mut dir_arg = OsString::from("/DIR=");
            dir_arg.push(&dir);
            cmd.arg(dir_arg);
            cmd.arg(if per_user(&dir) { "/CURRENTUSER" } else { "/ALLUSERS" });
            cmd.arg(format!("/WAITPID={}", std::process::id()));
            if relaunch {
                cmd.arg("/RELAUNCH=1");
            }
            cmd.spawn().map(drop).map_err(|e| format!("Couldn't start {}: {e}", setup.display()))
        }
        Pending::SwapFolder { dir, staged } => {
            swap_folder(&dir, &staged)?;
            if relaunch {
                Command::new(dir.join(EXE_NAME)).current_dir(&dir).spawn().map_err(|e| format!("Couldn't start the new version: {e}"))?;
            }
            Ok(())
        }
        Pending::SwapBundle { app, staged } => {
            let old = app.with_file_name(format!(".{}.old", app.file_stem().map_or_else(Default::default, |n| n.to_string_lossy())));
            let _ = std::fs::remove_dir_all(&old);
            std::fs::rename(&app, &old).map_err(|e| format!("{}: {e}", app.display()))?;
            if let Err(e) = std::fs::rename(&staged, &app) {
                let _ = std::fs::rename(&old, &app);
                return Err(format!("{}: {e}", app.display()));
            }
            if relaunch {
                Command::new("open").arg("-n").arg(&app).spawn().map_err(|e| format!("Couldn't start the new version: {e}"))?;
            }
            Ok(())
        }
    }
}

/// Whether an install folder belongs to the user alone (the setup's per-user mode puts it
/// under %LOCALAPPDATA%).
fn per_user(dir: &Path) -> bool {
    std::env::var_os("LOCALAPPDATA").is_some_and(|local| {
        let (dir, local) = (dir.to_string_lossy().to_lowercase(), local.to_string_lossy().to_lowercase());
        dir.starts_with(local.trim_end_matches('\\'))
    })
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for entry in std::fs::read_dir(dir.join(&rel))? {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                todo.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Moves the staged files into the install folder. Files being replaced move aside into
/// OLD_DIR (a running program can be renamed, though not overwritten, on Windows); the next
/// start deletes them. On failure everything moves back.
fn swap_folder(dir: &Path, staged: &Path) -> Result<(), String> {
    let old_dir = dir.join(OLD_DIR);
    let _ = std::fs::remove_dir_all(&old_dir);
    let files = files_under(staged).map_err(|e| format!("{}: {e}", staged.display()))?;
    // (new file, where the file it replaced went)
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut step = |rel: &Path| -> std::io::Result<()> {
        let (src, dest) = (staged.join(rel), dir.join(rel));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let aside = if dest.exists() {
            let aside = old_dir.join(rel);
            if let Some(parent) = aside.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&dest, &aside)?;
            Some(aside)
        } else {
            None
        };
        if let Err(e) = std::fs::rename(&src, &dest) {
            if let Some(aside) = &aside {
                let _ = std::fs::rename(aside, &dest);
            }
            return Err(e);
        }
        done.push((dest, aside));
        Ok(())
    };
    let result = files.iter().try_for_each(|rel| step(rel));
    if let Err(e) = result {
        for (dest, aside) in done.iter().rev() {
            let _ = std::fs::remove_file(dest);
            if let Some(aside) = aside {
                let _ = std::fs::rename(aside, dest);
            }
        }
        let _ = std::fs::remove_dir_all(staged);
        return Err(format!("Couldn't replace the files in {}: {e}", dir.display()));
    }
    let _ = std::fs::remove_dir_all(staged);
    Ok(())
}

/// At start: deletes what the last update left behind (the download, the replaced files)
/// and an update that was never installed.
pub fn clean_up() {
    if let Some(dir) = downloads_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
    match Install::this() {
        Ok(Install::Folder { dir }) => {
            let _ = std::fs::remove_dir_all(dir.join(OLD_DIR));
            let _ = std::fs::remove_dir_all(dir.join(STAGE_DIR));
        }
        Ok(Install::MacBundle { app }) => {
            let stem = app.file_stem().map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
            let _ = std::fs::remove_dir_all(app.with_file_name(format!(".{stem}.old")));
            let _ = std::fs::remove_dir_all(app.with_file_name(format!(".{stem}.update")));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions() {
        assert_eq!(Version::parse("v0.1.7"), Some(Version(0, 1, 7)));
        assert_eq!(Version::parse("0.2"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("1.0.0-beta.2"), Some(Version(1, 0, 0)));
        assert_eq!(Version::parse("latest"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert!(Version(0, 1, 10) > Version(0, 1, 9));
        assert!(Version(0, 2, 0) > Version(0, 1, 99));
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn reads_a_release_and_picks_the_package() {
        let json = serde_json::json!({
            "tag_name": "v9.0.1",
            "html_url": "https://github.com/alla2001/material-painter/releases/tag/v9.0.1",
            "body": "## What's Changed\n* Things\n",
            "assets": [
                {"name": "MaterialPainter-9.0.1-setup.exe", "browser_download_url": "https://x/setup.exe", "size": 10, "digest": "sha256:ABCD"},
                {"name": "MaterialPainter-9.0.1-setup.exe.sig", "browser_download_url": "https://x/setup.exe.sig", "size": 128},
                {"name": "MaterialPainter-9.0.1-windows-x64.zip", "browser_download_url": "https://x/w.zip", "size": 11},
                {"name": "MaterialPainter-9.0.1-linux-x64.tar.gz", "browser_download_url": "https://x/l.tar.gz", "size": 12},
                {"name": "MaterialPainter-9.0.1-macos-universal.dmg", "browser_download_url": "https://x/m.dmg", "size": 13},
            ]
        });
        let r = parse_release(&json).unwrap();
        assert_eq!(r.version, Version(9, 0, 1));
        assert_eq!(r.notes, "## What's Changed\n* Things");
        assert_eq!(r.assets[0].sha256.as_deref(), Some("abcd"));
        assert_eq!(r.assets[0].signature_url.as_deref(), Some("https://x/setup.exe.sig"));
        assert!(r.assets.iter().all(|a| !a.name.ends_with(".sig")));
        assert_eq!(r.assets[1].sha256, None);
        assert_eq!(r.assets[1].signature_url, None);
        // Another version's package doesn't count for this release.
        let mut old = r.clone();
        old.version = Version(9, 0, 2);
        assert!(Install::MacBundle { app: "/x.app".into() }.asset(&old).is_none());
        let pick = |i: Install| i.asset(&r).map(|a| a.name.clone());
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(pick(Install::WindowsSetup { dir: "C:/x".into() }).as_deref(), Some("MaterialPainter-9.0.1-setup.exe"));
            assert_eq!(pick(Install::Folder { dir: "C:/x".into() }).as_deref(), Some("MaterialPainter-9.0.1-windows-x64.zip"));
        }
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(pick(Install::Folder { dir: "/x".into() }).as_deref(), Some("MaterialPainter-9.0.1-linux-x64.tar.gz"));
        }
        assert_eq!(pick(Install::MacBundle { app: "/x.app".into() }).as_deref(), Some("MaterialPainter-9.0.1-macos-universal.dmg"));
    }

    #[test]
    fn checks_signatures() {
        use ring::signature::KeyPair;
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key = pair.public_key().as_ref();
        let (name, digest) = ("MaterialPainter-9.0.1-setup.exe", "ab".repeat(32));
        let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let sig = hex(pair.sign(signed_message(name, &digest).as_bytes()).as_ref());
        assert!(signature_ok(key, name, &digest, &sig));
        assert!(signature_ok(key, name, &digest, &format!("{sig}\n")));
        assert!(!signature_ok(key, name, &"cd".repeat(32), &sig), "other contents");
        assert!(!signature_ok(key, "MaterialPainter-9.0.0-setup.exe", &digest, &sig), "another package");
        assert!(!signature_ok(&SIGNING_KEY, name, &digest, &sig), "another key");
        assert!(!signature_ok(key, name, &digest, "zz"));
        assert_eq!(SIGNING_KEY[..2], [0x0e, 0x4d]);
    }

    #[test]
    fn archive_paths_stay_inside() {
        assert_eq!(inner_path("Material Painter/assets/ui/icon.png"), Some(PathBuf::from("assets/ui/icon.png")));
        assert_eq!(inner_path("Material Painter\\MaterialPainter.exe"), Some(PathBuf::from("MaterialPainter.exe")));
        assert_eq!(inner_path("./MaterialPainter-1-linux-x64/material-painter"), Some(PathBuf::from("material-painter")));
        assert_eq!(inner_path("Material Painter/"), None);
        assert_eq!(inner_path("x/../../evil"), None);
        assert_eq!(inner_path("x/C:/evil"), None);
    }

    #[test]
    fn knows_how_it_was_installed() {
        let root = std::env::temp_dir().join(format!("mp_update_detect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let folder = root.join("Material Painter");
        std::fs::create_dir_all(folder.join("assets")).unwrap();
        let exe = folder.join(EXE_NAME);
        if cfg!(target_os = "macos") {
            assert!(Install::detect_at(&exe).is_err());
            let app_exe = root.join("Material Painter.app/Contents/MacOS/MaterialPainter");
            assert_eq!(Install::detect_at(&app_exe), Ok(Install::MacBundle { app: root.join("Material Painter.app") }));
        } else {
            assert_eq!(Install::detect_at(&exe), Ok(Install::Folder { dir: folder.clone() }));
            if cfg!(windows) {
                std::fs::write(folder.join("unins000.exe"), b"").unwrap();
                assert_eq!(Install::detect_at(&exe), Ok(Install::WindowsSetup { dir: folder.clone() }));
            }
        }
        assert!(Install::detect_at(&root.join("elsewhere").join(EXE_NAME)).is_err());
        // A cargo build.
        std::fs::write(root.join("Cargo.toml"), b"").unwrap();
        assert!(Install::detect_at(&root.join("target/release").join(EXE_NAME)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swaps_folder_files_and_keeps_the_old_ones_aside() {
        let root = std::env::temp_dir().join(format!("mp_update_swap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let staged = root.join(STAGE_DIR);
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(staged.join("assets/new")).unwrap();
        std::fs::write(root.join(EXE_NAME), b"old exe").unwrap();
        std::fs::write(root.join("assets/kept.txt"), b"kept").unwrap();
        std::fs::write(staged.join(EXE_NAME), b"new exe").unwrap();
        std::fs::write(staged.join("assets/new/added.txt"), b"added").unwrap();
        swap_folder(&root, &staged).unwrap();
        assert_eq!(std::fs::read(root.join(EXE_NAME)).unwrap(), b"new exe");
        assert_eq!(std::fs::read(root.join("assets/new/added.txt")).unwrap(), b"added");
        assert_eq!(std::fs::read(root.join("assets/kept.txt")).unwrap(), b"kept");
        assert_eq!(std::fs::read(root.join(OLD_DIR).join(EXE_NAME)).unwrap(), b"old exe");
        assert!(!staged.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unpacks_a_zip_package() {
        let root = std::env::temp_dir().join(format!("mp_update_zip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let package = root.join("MaterialPainter-9.0.0-windows-x64.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.add_directory("Material Painter/", opts).unwrap();
            zip.start_file(format!("Material Painter/{EXE_NAME}"), opts).unwrap();
            zip.write_all(b"exe").unwrap();
            // Windows PowerShell 5 writes backslashes.
            zip.start_file("Material Painter\\assets\\ui\\icon.png", opts).unwrap();
            zip.write_all(b"png").unwrap();
            zip.start_file("Material Painter/../evil.txt", opts).unwrap();
            zip.write_all(b"no").unwrap();
            zip.finish().unwrap();
        }
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        unpack(&package, &out).unwrap();
        assert_eq!(std::fs::read(out.join(EXE_NAME)).unwrap(), b"exe");
        assert_eq!(std::fs::read(out.join("assets/ui/icon.png")).unwrap(), b"png");
        assert!(!root.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The whole update of an unpacked install with the latest real release package
    /// (network, ~90 MB): download with digest check, unpack, swap.
    #[test]
    #[ignore]
    fn updates_a_folder_install_from_the_latest_release() {
        let release = latest(&agent()).unwrap();
        let root = std::env::temp_dir().join(format!("mp_update_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("Material Painter");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join(EXE_NAME), b"old").unwrap();
        let install = Install::Folder { dir: dir.clone() };
        let asset = install.asset(&release).expect("a package for this system").clone();
        assert!(asset.sha256.is_some(), "GitHub lists a digest");
        let last = std::sync::atomic::AtomicU64::new(0);
        // The release's own signature when it has one, else one made here for its digest.
        let package = if asset.signature_url.is_some() {
            download(&agent(), &asset, &AtomicBool::new(false), &|done, _| last.store(done, Ordering::Relaxed)).unwrap()
        } else {
            assert_eq!(download(&agent(), &asset, &AtomicBool::new(false), &|_, _| {}).unwrap_err(), UNSIGNED);
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
            let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
            let verify = |digest: &str| {
                use ring::signature::KeyPair;
                let sig: String = pair.sign(signed_message(&asset.name, digest).as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect();
                signature_ok(pair.public_key().as_ref(), &asset.name, digest, &sig).then_some(()).ok_or_else(|| "bad signature".to_string())
            };
            fetch_package(&agent(), &asset, &verify, &AtomicBool::new(false), &|done, _| last.store(done, Ordering::Relaxed)).unwrap()
        };
        assert_eq!(last.load(Ordering::Relaxed), asset.size);
        let Pending::SwapFolder { staged, .. } = prepare(&install, &package).unwrap() else { panic!("a folder swap") };
        swap_folder(&dir, &staged).unwrap();
        let exe = std::fs::metadata(dir.join(EXE_NAME)).unwrap().len();
        println!("{} -> program {exe} bytes", asset.name);
        assert!(exe > 1 << 20);
        assert!(dir.join("assets/ui/icon.png").is_file());
        assert!(dir.join("THIRD_PARTY_NOTICES.html").is_file());
        // A corrupted download is refused.
        let bad = Asset { sha256: Some("00".repeat(32)), ..asset };
        assert!(fetch_package(&agent(), &bad, &|_| Ok(()), &AtomicBool::new(false), &|_, _| {}).unwrap_err().contains("checksum"));
        // A package the release key didn't sign is refused and deleted.
        let _ = std::fs::remove_file(&package);
        let forged = Asset { signature_url: Some(format!("{}.sig", bad.url)), sha256: None, ..bad };
        let refused = fetch_package(&agent(), &forged, &|_| Err("unsigned".into()), &AtomicBool::new(false), &|_, _| {});
        assert_eq!(refused.unwrap_err(), "unsigned");
        assert!(!downloads_dir().unwrap().join(&forged.name).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Checks a package signed by tools/update-sign against the app's key:
    /// MP_SIGNED=<package> (its .sig beside it).
    #[test]
    #[ignore]
    fn verifies_a_package_signed_by_the_tool() {
        let Some(path) = std::env::var_os("MP_SIGNED").map(PathBuf::from) else { return };
        let bytes = std::fs::read(&path).unwrap();
        let digest: String = ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let mut sig = path.clone().into_os_string();
        sig.push(".sig");
        let sig = std::fs::read_to_string(sig).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(signature_ok(&SIGNING_KEY, &name, &digest, &sig), "{name} isn't signed with the release key");
        assert!(!signature_ok(&SIGNING_KEY, &format!("{name}x"), &digest, &sig));
        println!("{name}: signed with the release key");
    }

    /// Asks GitHub for the latest release (network).
    #[test]
    #[ignore]
    fn probe_latest_release() {
        let r = latest(&agent()).unwrap();
        println!("latest {} ({} assets), this build {}", r.version, r.assets.len(), Version::current());
        for a in &r.assets {
            println!("  {} {} bytes sha256 {:?}", a.name, a.size, a.sha256);
        }
        if let Ok(install) = Install::detect() {
            println!("this install: {install:?} -> {:?}", install.asset(&r).map(|a| &a.name));
        }
    }
}
