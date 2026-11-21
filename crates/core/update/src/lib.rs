//! Updates of Bowyer. A feed is a JSON file at a fixed address that names the newest version
//! and, for each platform, a package with its minisign signature (the format cargo-packager's
//! signer writes). A package is installed only when its signature matches the public key built
//! into the program; without a key the window only says that a version is out.
//!
//! Files come over the system's `curl` (Windows 10 and later, macOS and Linux have it), so the
//! program carries no TLS of its own. Plain `http` is accepted for this computer only, for tests.
//!
//! How a package goes in depends on how Bowyer was installed: the NSIS installer runs again in
//! passive mode (it closes the running program, installs and starts it), an AppImage is
//! replaced by the new one, a macOS bundle by the one in the `.app.tar.gz`. Anything else (a
//! `.deb`, a build from source) only learns that a version is out.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use serde::Deserialize;

/// The feed: the newest version and its packages by platform (see [`platform`]).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pub_date: Option<String>,
    /// A page about the release, for systems without a package.
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub platforms: HashMap<String, Package>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub url: String,
    /// The minisign signature of the file: its text, or that text in base64.
    pub signature: String,
    /// `nsis`, `appimage` or `app`; guessed from the address when missing.
    #[serde(default)]
    pub format: Option<String>,
}

impl Package {
    pub fn format(&self) -> &str {
        if let Some(format) = &self.format {
            return format;
        }
        let url = self.url.to_lowercase();
        if url.ends_with(".exe") {
            "nsis"
        } else if url.ends_with(".appimage") {
            "appimage"
        } else if url.ends_with(".app.tar.gz") {
            "app"
        } else {
            ""
        }
    }
}

/// A version newer than the running one.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: semver::Version,
    pub notes: String,
    pub page: Option<String>,
    /// The package for this platform, when the feed has one.
    pub package: Option<Package>,
}

#[derive(Debug)]
pub enum Error {
    /// The address is not `https`, nor `http` on this computer.
    Address(String),
    Io(io::Error),
    /// `curl` failed: its code and what it said.
    Transfer(Option<i32>, String),
    Feed(String),
    Signature(String),
    /// The package cannot go in the way Bowyer was installed.
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Address(url) => write!(f, "not a secure address: {url}"),
            Error::Io(error) => write!(f, "{error}"),
            Error::Transfer(Some(code), text) => write!(f, "download failed (curl {code}): {}", text.trim()),
            Error::Transfer(None, text) => write!(f, "download failed: {}", text.trim()),
            Error::Feed(text) => write!(f, "the update feed is wrong: {text}"),
            Error::Signature(text) => write!(f, "the signature does not match: {text}"),
            Error::Unsupported => write!(f, "this installation cannot update itself"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The key of this platform in [`Manifest::platforms`]: `windows-x86_64`, `linux-x86_64`,
/// `macos-aarch64` and so on.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The release in `manifest` when it is newer than `current`.
pub fn newer(manifest: &Manifest, current: &str, platform: &str) -> Result<Option<Release>> {
    let version = semver::Version::parse(manifest.version.trim_start_matches('v'))
        .map_err(|error| Error::Feed(format!("version {:?}: {error}", manifest.version)))?;
    let current = semver::Version::parse(current.trim_start_matches('v'))
        .map_err(|error| Error::Feed(format!("running version {current:?}: {error}")))?;
    if version <= current {
        return Ok(None);
    }
    Ok(Some(Release {
        version,
        notes: manifest.notes.clone(),
        page: manifest.page.clone(),
        package: manifest.platforms.get(platform).cloned(),
    }))
}

/// Text that is either itself or base64 of itself, as signatures and keys travel both ways.
fn plain_or_base64(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("untrusted comment:") {
        return trimmed.to_owned();
    }
    base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|decoded| decoded.starts_with("untrusted comment:"))
        .unwrap_or_else(|| trimmed.to_owned())
}

/// Checks `data` against a minisign `signature` made by the key `public_key`: the key's file,
/// its line `RW…`, or either in base64.
pub fn verify(data: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let key_text = plain_or_base64(public_key);
    let key = if key_text.contains('\n') {
        minisign_verify::PublicKey::decode(&key_text)
    } else {
        minisign_verify::PublicKey::from_base64(key_text.trim())
    }
    .map_err(|error| Error::Signature(format!("public key: {error}")))?;
    let signature = minisign_verify::Signature::decode(&plain_or_base64(signature))
        .map_err(|error| Error::Signature(format!("signature: {error}")))?;
    key.verify(data, &signature, false).map_err(|error| Error::Signature(error.to_string()))
}

/// Whether `url` may be fetched: `https`, or `http` on this computer.
pub fn allowed(url: &str) -> bool {
    let lower = url.to_lowercase();
    if lower.starts_with("https://") {
        return true;
    }
    let Some(rest) = lower.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // A user part (`127.0.0.1:1@host`) or a backslash would send curl elsewhere.
    if authority.contains(['@', '\\']) {
        return false;
    }
    let host = authority;
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// `curl` of the system: System32's on Windows, so no other one on `PATH` stands in.
fn curl() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let system = std::env::var_os("SystemRoot").map(|root| PathBuf::from(root).join("System32").join("curl.exe"));
        let mut command = Command::new(system.filter(|path| path.is_file()).unwrap_or_else(|| PathBuf::from("curl")));
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("curl")
    }
}

fn transfer(url: &str, out: Option<&Path>) -> Result<Vec<u8>> {
    if !allowed(url) {
        return Err(Error::Address(url.to_owned()));
    }
    let protocols = if url.to_lowercase().starts_with("https://") { "=https" } else { "=http,https" };
    let mut command = curl();
    command.args(["--fail", "--silent", "--show-error", "--location", "--max-redirs", "5", "--connect-timeout", "15", "--max-time", "600"]);
    command.args(["--proto", protocols, "--proto-redir", "=https"]);
    if let Some(out) = out {
        command.arg("--output").arg(out);
    }
    command.arg("--").arg(url);
    let output = command.output()?;
    if !output.status.success() {
        return Err(Error::Transfer(output.status.code(), String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    Ok(output.stdout)
}

/// Reads the feed at `feed` and returns the release newer than `current`, if any.
pub fn check(feed: &str, current: &str) -> Result<Option<Release>> {
    let body = transfer(feed, None)?;
    let manifest: Manifest = serde_json::from_slice(&body).map_err(|error| Error::Feed(error.to_string()))?;
    newer(&manifest, current, &platform())
}

/// The name of the file a package is saved as: the last part of its address, without anything
/// that could leave the folder.
fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    let name = name.trim_start_matches('.');
    if name.is_empty() { "bowyer-update".to_owned() } else { name.to_owned() }
}

/// Downloads `package` into `dir` and checks its signature with `public_key`. A file whose
/// signature does not match is deleted.
pub fn download(package: &Package, dir: &Path, public_key: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(&package.url));
    let partial = path.with_extension("part");
    let _ = std::fs::remove_file(&partial);
    transfer(&package.url, Some(&partial))?;
    let data = std::fs::read(&partial)?;
    if let Err(error) = verify(&data, &package.signature, public_key) {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// How the running program was installed, which decides how an update goes in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// By the NSIS installer of cargo-packager, into this folder.
    Nsis(PathBuf),
    /// As this AppImage.
    AppImage(PathBuf),
    /// As this macOS bundle.
    MacBundle(PathBuf),
    /// Some other way: a `.deb`, a build from source.
    Unmanaged,
}

impl Install {
    /// The package format that replaces this installation.
    pub fn format(&self) -> Option<&'static str> {
        match self {
            Install::Nsis(_) => Some("nsis"),
            Install::AppImage(_) => Some("appimage"),
            Install::MacBundle(_) => Some("app"),
            Install::Unmanaged => None,
        }
    }

    /// Where a package for this installation is downloaded: next to an AppImage, so it can
    /// take its place in one rename; the cache otherwise.
    pub fn download_dir(&self, cache: &Path) -> PathBuf {
        match self {
            Install::AppImage(path) => path.parent().map(Path::to_path_buf).unwrap_or_else(|| cache.to_path_buf()),
            _ => cache.join("updates"),
        }
    }
}

/// How this program was installed.
pub fn install_kind() -> Install {
    let Ok(exe) = std::env::current_exe() else { return Install::Unmanaged };
    install_kind_of(&exe, std::env::var_os("APPIMAGE").map(PathBuf::from))
}

fn install_kind_of(exe: &Path, appimage: Option<PathBuf>) -> Install {
    if let Some(appimage) = appimage.filter(|path| path.is_file()) {
        return Install::AppImage(appimage);
    }
    let dir = exe.parent().unwrap_or(Path::new(""));
    // cargo-packager's installer leaves its uninstaller next to the program.
    if cfg!(windows) && dir.join("uninstall.exe").is_file() {
        return Install::Nsis(dir.to_path_buf());
    }
    // …/Bowyer.app/Contents/MacOS/bowyer
    let bundle = dir.parent().and_then(Path::parent).filter(|path| path.extension().is_some_and(|ext| ext == "app"));
    if cfg!(target_os = "macos")
        && let Some(bundle) = bundle
    {
        return Install::MacBundle(bundle.to_path_buf());
    }
    Install::Unmanaged
}

/// Puts the downloaded `package` in place of the installation `install` and starts the new
/// version; the caller quits right after. On Windows the installer closes what is left.
pub fn apply(install: &Install, package: &Path) -> Result<()> {
    match install {
        Install::Nsis(_) => {
            let mut command = Command::new(package);
            // Passive: a progress window, no questions; no new shortcuts; start Bowyer after.
            command.args(["/P", "/NS", "/R"]);
            detach(&mut command);
            command.spawn()?;
            Ok(())
        }
        Install::AppImage(target) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(package, std::fs::Permissions::from_mode(0o755))?;
            }
            // The running AppImage keeps its open file; the name points to the new one.
            std::fs::rename(package, target)?;
            let mut command = Command::new(target);
            detach(&mut command);
            command.spawn()?;
            Ok(())
        }
        Install::MacBundle(bundle) => {
            let unpacked = package.with_extension("unpacked");
            let _ = std::fs::remove_dir_all(&unpacked);
            std::fs::create_dir_all(&unpacked)?;
            let status = Command::new("tar").arg("-xzf").arg(package).arg("-C").arg(&unpacked).status()?;
            if !status.success() {
                return Err(Error::Io(io::Error::other("tar could not unpack the update")));
            }
            let new_bundle = std::fs::read_dir(&unpacked)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .find(|path| path.extension().is_some_and(|ext| ext == "app"))
                .ok_or_else(|| Error::Io(io::Error::other("no .app in the update")))?;
            let old = bundle.with_extension("app.old");
            let _ = std::fs::remove_dir_all(&old);
            std::fs::rename(bundle, &old)?;
            if let Err(error) = std::fs::rename(&new_bundle, bundle) {
                let _ = std::fs::rename(&old, bundle);
                return Err(error.into());
            }
            let _ = std::fs::remove_dir_all(&old);
            let _ = std::fs::remove_dir_all(&unpacked);
            // `open` starts the new bundle once this process is gone.
            let mut command = Command::new("sh");
            command.arg("-c").arg("sleep 1; open -n \"$0\"").arg(bundle);
            detach(&mut command);
            command.spawn()?;
            Ok(())
        }
        Install::Unmanaged => Err(Error::Unsupported),
    }
}

/// A child that outlives this process, with no console of its own.
fn detach(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
}

#[cfg(test)]
mod tests;
