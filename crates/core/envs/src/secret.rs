//! Passwords of SSH hosts, kept by the system rather than by Bowyer:
//!
//! - Windows: Credential Manager, one generic credential per host named `Bowyer SSH/<host>`;
//! - macOS: the Keychain, a generic password of service `Bowyer SSH` and account `<host>`;
//! - Linux and other Unix desktops: the Secret Service of the session (GNOME Keyring,
//!   KWallet, KeePassXC), an item labelled `Bowyer SSH/<host>` with the attributes
//!   `application = bowyer` and `host = <host>` in the default collection.
//!
//! Nothing of them reaches Bowyer's own files or the command lines of its programs: `ssh`
//! asks for the password through Bowyer as its askpass program, and that reads the password
//! here (see [`crate::askpass_answer`]).

use std::io;
use std::sync::{Mutex, MutexGuard};

/// One call to the store at a time in this process: concurrent writes and deletes from
/// several threads were seen to leave a deleted credential behind.
static STORE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    STORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The name a password goes by in the store.
fn label(host: &str) -> String {
    format!("Bowyer SSH/{host}")
}

/// Which store keeps the passwords here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    CredentialManager,
    Keychain,
    SecretService,
}

/// The store of this system, or `None` when it has none (a Linux session without a Secret
/// Service): its hosts sign in with keys only.
pub fn store_kind() -> Option<Store> {
    imp::kind()
}

/// Whether this system keeps passwords for Bowyer.
pub fn supported() -> bool {
    store_kind().is_some()
}

/// Keeps the password of `host`, in place of one kept before.
pub fn store(host: &str, password: &str) -> io::Result<()> {
    let _one = serial();
    imp::store(host, password)
}

/// The password kept for `host`. A locked keyring may ask the user to unlock it.
pub fn load(host: &str) -> Option<String> {
    let _one = serial();
    imp::load(host)
}

/// Whether a password is kept for `host`: such a host signs in with it. Never asks to
/// unlock anything.
pub fn exists(host: &str) -> bool {
    let _one = serial();
    imp::exists(host)
}

/// Forgets the password of `host`; nothing happens when none is kept.
pub fn remove(host: &str) -> io::Result<()> {
    let _one = serial();
    imp::remove(host)
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
    };

    use super::{Store, label};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub fn kind() -> Option<Store> {
        Some(Store::CredentialManager)
    }

    pub fn store(host: &str, password: &str) -> io::Result<()> {
        let mut target = wide(&label(host));
        let mut user = wide(host);
        let mut blob = password.as_bytes().to_vec();
        // SAFETY: an all-zero CREDENTIALW is a valid empty value; the buffers outlive the call.
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target.as_mut_ptr();
        credential.UserName = user.as_mut_ptr();
        credential.CredentialBlobSize = blob.len() as u32;
        credential.CredentialBlob = blob.as_mut_ptr();
        // The credential stays for this user on this computer and does not roam.
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        // SAFETY: `credential` points at live buffers.
        let written = unsafe { CredWriteW(&credential, 0) };
        blob.fill(0);
        if written == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    pub fn load(host: &str) -> Option<String> {
        let target = wide(&label(host));
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: CredReadW fills `credential` with a block that CredFree releases.
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 || credential.is_null() {
            return None;
        }
        // SAFETY: the blob is `CredentialBlobSize` bytes long while the block lives.
        let password = unsafe {
            let read = &*credential;
            let bytes = if read.CredentialBlob.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(read.CredentialBlob, read.CredentialBlobSize as usize)
            };
            String::from_utf8(bytes.to_vec()).ok()
        };
        // SAFETY: the block came from CredReadW.
        unsafe { CredFree(credential.cast()) };
        password
    }

    pub fn exists(host: &str) -> bool {
        load(host).is_some()
    }

    pub fn remove(host: &str) -> io::Result<()> {
        let target = wide(&label(host));
        // SAFETY: a NUL-terminated name.
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            // SAFETY: reads the error of the call above on this thread.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io;

    use security_framework::passwords;

    use super::{Store, label};

    const SERVICE: &str = "Bowyer SSH";
    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn kind() -> Option<Store> {
        Some(Store::Keychain)
    }

    pub fn store(host: &str, password: &str) -> io::Result<()> {
        // The name Keychain Access shows; service and account find the item.
        let mut options = passwords::PasswordOptions::new_generic_password(SERVICE, host);
        options.set_label(&label(host));
        passwords::set_generic_password_options(password.as_bytes(), options).map_err(io::Error::other)
    }

    pub fn load(host: &str) -> Option<String> {
        passwords::generic_password(passwords::PasswordOptions::new_generic_password(SERVICE, host))
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    }

    pub fn exists(host: &str) -> bool {
        load(host).is_some()
    }

    pub fn remove(host: &str) -> io::Result<()> {
        match passwords::delete_generic_password(SERVICE, host) {
            Err(error) if error.code() == NOT_FOUND => Ok(()),
            result => result.map_err(io::Error::other),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    //! The Secret Service API (freedesktop.org) over the session bus, with the `plain`
    //! algorithm: the bus is the user's own, as with every client of the keyring.

    use std::collections::HashMap;
    use std::io;
    use std::sync::OnceLock;

    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    use super::{Store, label};

    const SERVICE: &str = "org.freedesktop.secrets";
    const SERVICE_PATH: &str = "/org/freedesktop/secrets";
    /// The empty object path: no prompt, no collection.
    const NO_PROMPT: &str = "/";

    /// A secret as the API passes it: session, parameters, value, content type.
    type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

    fn error(error: zbus::Error) -> io::Error {
        io::Error::other(error)
    }

    fn bus() -> io::Result<Connection> {
        Connection::session().map_err(error)
    }

    fn service(bus: &Connection) -> io::Result<Proxy<'static>> {
        Proxy::new(bus, SERVICE, SERVICE_PATH, "org.freedesktop.Secret.Service").map_err(error)
    }

    fn attributes(host: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", "bowyer"), ("host", host)])
    }

    /// Whether a Secret Service runs on the session bus or starts when called.
    pub fn kind() -> Option<Store> {
        static FOUND: OnceLock<bool> = OnceLock::new();
        let found = *FOUND.get_or_init(|| {
            let Ok(bus) = bus() else { return false };
            let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&bus) else { return false };
            let name = zbus::names::BusName::try_from(SERVICE).expect("a valid bus name");
            dbus.name_has_owner(name.clone()).unwrap_or(false)
                || dbus.list_activatable_names().is_ok_and(|names| names.iter().any(|known| known.as_str() == SERVICE))
        });
        found.then_some(Store::SecretService)
    }

    /// A session for passing secrets in the clear over the user's own bus.
    fn open_session(service: &Proxy<'_>) -> io::Result<OwnedObjectPath> {
        let (_, session): (OwnedValue, OwnedObjectPath) = service.call("OpenSession", &("plain", Value::from(""))).map_err(error)?;
        Ok(session)
    }

    /// Runs a prompt of the service (unlocking a keyring, confirming a deletion) and waits
    /// for the user. `Ok(false)` when they dismissed it.
    fn prompt(bus: &Connection, path: &OwnedObjectPath) -> io::Result<bool> {
        if path.as_str() == NO_PROMPT {
            return Ok(true);
        }
        let prompt = Proxy::new(bus, SERVICE, path.as_str(), "org.freedesktop.Secret.Prompt").map_err(error)?;
        let mut completed = prompt.receive_signal("Completed").map_err(error)?;
        prompt.call_method("Prompt", &("",)).map_err(error)?;
        let Some(message) = completed.next() else { return Ok(false) };
        let (dismissed, _): (bool, OwnedValue) = message.body().deserialize().map_err(error)?;
        Ok(!dismissed)
    }

    /// The items kept for `host`: unlocked, then locked.
    fn search(service: &Proxy<'_>, host: &str) -> io::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        service.call("SearchItems", &(attributes(host),)).map_err(error)
    }

    pub fn store(host: &str, password: &str) -> io::Result<()> {
        let bus = bus()?;
        let service = service(&bus)?;
        // The default keyring, opened for the user first when it is locked.
        let default: OwnedObjectPath = service.call("ReadAlias", &("default",)).map_err(error)?;
        if default.as_str() == NO_PROMPT {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the keyring has no default collection"));
        }
        let (_, unlock): (Vec<OwnedObjectPath>, OwnedObjectPath) = service.call("Unlock", &(vec![default.clone()],)).map_err(error)?;
        if !prompt(&bus, &unlock)? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the keyring stayed locked"));
        }
        let session = open_session(&service)?;
        let collection = Proxy::new(&bus, SERVICE, default.as_str(), "org.freedesktop.Secret.Collection").map_err(error)?;
        let properties: HashMap<&str, Value<'_>> = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(label(host))),
            ("org.freedesktop.Secret.Item.Attributes", Value::from(attributes(host))),
        ]);
        let secret: Secret = (session.clone(), Vec::new(), password.as_bytes().to_vec(), "text/plain; charset=utf8".into());
        let (_, created): (OwnedObjectPath, OwnedObjectPath) = collection.call("CreateItem", &(properties, secret, true)).map_err(error)?;
        let stored = prompt(&bus, &created);
        close(&bus, &session);
        if stored? { Ok(()) } else { Err(io::Error::new(io::ErrorKind::PermissionDenied, "the keyring refused the password")) }
    }

    pub fn load(host: &str) -> Option<String> {
        let bus = bus().ok()?;
        let service = service(&bus).ok()?;
        let (mut unlocked, locked) = search(&service, host).ok()?;
        if unlocked.is_empty() && !locked.is_empty() {
            let (opened, unlock): (Vec<OwnedObjectPath>, OwnedObjectPath) = service.call("Unlock", &(locked,)).ok()?;
            if unlock.as_str() == NO_PROMPT {
                unlocked = opened;
            } else if prompt(&bus, &unlock).ok()? {
                unlocked = search(&service, host).ok()?.0;
            }
        }
        let item = unlocked.into_iter().next()?;
        let session = open_session(&service).ok()?;
        let secrets: HashMap<OwnedObjectPath, Secret> = service.call("GetSecrets", &(vec![item], &session)).ok()?;
        close(&bus, &session);
        let (_, _, value, _) = secrets.into_values().next()?;
        String::from_utf8(value).ok()
    }

    pub fn exists(host: &str) -> bool {
        let Ok(bus) = bus() else { return false };
        let Ok(service) = service(&bus) else { return false };
        search(&service, host).is_ok_and(|(unlocked, locked)| !unlocked.is_empty() || !locked.is_empty())
    }

    pub fn remove(host: &str) -> io::Result<()> {
        // No store, nothing kept.
        if kind().is_none() {
            return Ok(());
        }
        let bus = bus()?;
        let service = service(&bus)?;
        let (unlocked, locked) = search(&service, host)?;
        for item in unlocked.into_iter().chain(locked) {
            let item = Proxy::new(&bus, SERVICE, item.as_str(), "org.freedesktop.Secret.Item").map_err(error)?;
            let asked: OwnedObjectPath = item.call("Delete", &()).map_err(error)?;
            prompt(&bus, &asked)?;
        }
        Ok(())
    }

    fn close(bus: &Connection, session: &OwnedObjectPath) {
        if let Ok(session) = Proxy::new(bus, SERVICE, session.as_str(), "org.freedesktop.Secret.Session") {
            let _ = session.call_method("Close", &());
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    use std::io;

    use super::Store;

    pub fn kind() -> Option<Store> {
        None
    }

    pub fn store(_host: &str, _password: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no password store on this system"))
    }

    pub fn load(_host: &str) -> Option<String> {
        None
    }

    pub fn exists(_host: &str) -> bool {
        false
    }

    pub fn remove(_host: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Whether the store answers for this process: a service account without a logon session
/// (some CI runners on Windows), a locked or missing keyring elsewhere, has none, and tests
/// that need it skip then.
#[cfg(test)]
pub(crate) fn usable_in_tests() -> bool {
    if !supported() {
        eprintln!("skipped: no password store here");
        return false;
    }
    // A name of its own per call: tests run side by side.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = format!("bowyer-probe-{}-{call}@example.invalid", std::process::id());
    match store(&probe, "probe") {
        Ok(()) => remove(&probe).is_ok(),
        // ERROR_NO_SUCH_LOGON_SESSION
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(1312) => {
            eprintln!("skipped: Credential Manager has no logon session here");
            false
        }
        Err(error) if cfg!(windows) => panic!("Credential Manager: {error}"),
        // A CI runner's keychain or keyring may be locked or refuse without a user.
        Err(error) => {
            eprintln!("skipped: the password store refused: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_is_kept_read_and_forgotten() {
        if !usable_in_tests() {
            return;
        }
        let host = format!("bowyer-test-{}@example.invalid", std::process::id());
        assert!(!exists(&host));
        store(&host, "first pässwörd 密码").unwrap();
        assert!(exists(&host));
        store(&host, "second").unwrap();
        assert_eq!(load(&host).as_deref(), Some("second"), "a new password takes the old one's place");
        remove(&host).unwrap();
        assert_eq!(load(&host), None);
        assert!(!exists(&host));
        remove(&host).unwrap();
    }
}
