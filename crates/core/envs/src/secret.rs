//! Passwords of SSH hosts, kept by the system: Windows Credential Manager, one generic
//! credential per host named `Bowyer SSH/<host>`. Nothing of them reaches Bowyer's own files
//! or the command lines of its programs: `ssh` asks for the password through Bowyer as its
//! askpass program, and that reads the credential (see [`crate::askpass_answer`]).
//! Other systems have no store here yet, so their hosts sign in with keys only.

use std::io;
use std::sync::{Mutex, MutexGuard};

/// One call to the store at a time in this process: concurrent writes and deletes from
/// several threads were seen to leave a deleted credential behind.
static STORE: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    STORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The name of the credential of `host`.
fn target(host: &str) -> String {
    format!("Bowyer SSH/{host}")
}

/// Whether this system keeps passwords for Bowyer.
pub fn supported() -> bool {
    cfg!(windows)
}

/// Keeps the password of `host`, in place of one kept before.
pub fn store(host: &str, password: &str) -> io::Result<()> {
    let _one = serial();
    imp::store(&target(host), host, password)
}

/// The password kept for `host`.
pub fn load(host: &str) -> Option<String> {
    let _one = serial();
    imp::load(&target(host))
}

/// Whether a password is kept for `host`: such a host signs in with it.
pub fn exists(host: &str) -> bool {
    load(host).is_some()
}

/// Forgets the password of `host`; nothing happens when none is kept.
pub fn remove(host: &str) -> io::Result<()> {
    let _one = serial();
    imp::remove(&target(host))
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub fn store(target: &str, user: &str, password: &str) -> io::Result<()> {
        let mut target = wide(target);
        let mut user = wide(user);
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

    pub fn load(target: &str) -> Option<String> {
        let target = wide(target);
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

    pub fn remove(target: &str) -> io::Result<()> {
        let target = wide(target);
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

#[cfg(not(windows))]
mod imp {
    use std::io;

    pub fn store(_target: &str, _user: &str, _password: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no password store on this system"))
    }

    pub fn load(_target: &str) -> Option<String> {
        None
    }

    pub fn remove(_target: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Whether the store answers for this process: a service account without a logon session
/// (some CI runners) has none, and tests that need it skip then.
#[cfg(all(test, windows))]
pub(crate) fn usable_in_tests() -> bool {
    // A name of its own per call: tests run side by side.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = format!("bowyer-probe-{}-{call}@example.invalid", std::process::id());
    match store(&probe, "probe") {
        Ok(()) => remove(&probe).is_ok(),
        // ERROR_NO_SUCH_LOGON_SESSION
        Err(error) if error.raw_os_error() == Some(1312) => {
            eprintln!("skipped: Credential Manager has no logon session here");
            false
        }
        Err(error) => panic!("Credential Manager: {error}"),
    }
}

#[cfg(all(test, windows))]
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
        store(&host, "second").unwrap();
        assert_eq!(load(&host).as_deref(), Some("second"), "a new password takes the old one's place");
        remove(&host).unwrap();
        assert_eq!(load(&host), None);
        remove(&host).unwrap();
    }
}
