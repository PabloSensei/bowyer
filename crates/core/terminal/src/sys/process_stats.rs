//! The process table of the machine and the facts about one process that a walk of a tree
//! needs: its creation time and whether it is a console program. The table is read from a
//! toolhelp snapshot: no WMI and no call that waits for a process.
//!
//! The file names no other module of the crate, so a test can include it as it is.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};

/// The `Subsystem` field of a PE optional header: a window program, or a console program.
const IMAGE_SUBSYSTEM_WINDOWS_GUI: u16 = 2;
const IMAGE_SUBSYSTEM_WINDOWS_CUI: u16 = 3;
/// The optional header starts after the PE signature and the 20-byte COFF header.
const PE_OPTIONAL_HEADER: u64 = 24;
/// `Subsystem` lies at the same offset in both the 32-bit and the 64-bit optional header.
const PE_SUBSYSTEM: usize = 68;

/// One live process of the machine, as a toolhelp snapshot reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessEntry {
    pub(crate) pid: u32,
    pub(crate) parent: Option<u32>,
    pub(crate) name: String,
}

/// Every live process with its parent. The id of a dead parent is kept, so a program
/// that outlived its shell still names it, and a walk can find it after the shell is gone.
pub(crate) fn process_table() -> Vec<ProcessEntry> {
    snapshot()
        .into_iter()
        .map(|entry| ProcessEntry {
            pid: entry.pid,
            parent: (entry.parent != 0).then_some(entry.parent),
            name: entry.name,
        })
        .collect()
}

/// When the process was created, in the units of `FILETIME`. Two processes never share
/// it, so a caller can tell its own process from one that took the same id later.
pub(crate) fn process_created(pid: u32) -> Option<u64> {
    // SAFETY: the handle is checked and closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a non-null result is ours to own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    creation_time(handle.as_raw_handle())
}

/// Whether the image behind `pid` is a console program, from the `Subsystem` field of its
/// PE header. `None` when the image cannot be read, and the caller then leaves the process
/// alone: ending a program that might not be a console one is worse than one left running.
///
/// The subsystem belongs to the file, so the answer is cached by image path.
pub(crate) fn is_console_process(pid: u32) -> Option<bool> {
    let path = image_path(pid)?;
    let mut cache = subsystems()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(known) = cache.get(&path) {
        return *known;
    }
    let console = subsystem_of(&path);
    cache.insert(path, console);
    console
}

fn subsystems() -> &'static Mutex<HashMap<PathBuf, Option<bool>>> {
    static SUBSYSTEMS: OnceLock<Mutex<HashMap<PathBuf, Option<bool>>>> = OnceLock::new();
    SUBSYSTEMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The image path of a running process, as the loader opened it.
fn image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: the handle is checked and closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: a non-null result is ours to own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: the buffer is writable for `length` characters, and the handle is valid.
    let done = unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle(), 0, buffer.as_mut_ptr(), &mut length)
    };
    (done != 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..length as usize])))
}

/// Reads the PE header of an image: `true` for a console program, `false` for a window
/// program, `None` for any other subsystem or a header that cannot be read.
fn subsystem_of(path: &Path) -> Option<bool> {
    let mut file = File::open(path).ok()?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).ok()?;
    // `e_lfanew` at 0x3c points at the PE signature.
    let offset = u32::from_le_bytes([dos[0x3c], dos[0x3d], dos[0x3e], dos[0x3f]]) as u64;
    file.seek(SeekFrom::Start(offset + PE_OPTIONAL_HEADER))
        .ok()?;
    // The magic tells PE32 from PE32+; `Subsystem` follows at the same offset in both.
    let mut optional = [0u8; PE_SUBSYSTEM + 2];
    file.read_exact(&mut optional).ok()?;
    let magic = u16::from_le_bytes([optional[0], optional[1]]);
    if magic != 0x10b && magic != 0x20b {
        return None;
    }
    match u16::from_le_bytes([optional[PE_SUBSYSTEM], optional[PE_SUBSYSTEM + 1]]) {
        IMAGE_SUBSYSTEM_WINDOWS_GUI => Some(false),
        IMAGE_SUBSYSTEM_WINDOWS_CUI => Some(true),
        _ => None,
    }
}

/// Ends one process, but only while the id still names the one created at `created`.
/// Returns `Ok` when the process is already gone or the id was reused.
pub(crate) fn terminate_process(pid: u32, created: u64) -> io::Result<()> {
    // SAFETY: the handle is checked and closed below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Ok(()),
            _ => Err(error),
        };
    }
    // SAFETY: a non-null result is ours to own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if creation_time(handle.as_raw_handle()) != Some(created) {
        // The id belongs to another process now, which is not ours to end.
        return Ok(());
    }
    // SAFETY: the handle names a process that is still the one we opened.
    if unsafe { TerminateProcess(handle.as_raw_handle(), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn creation_time(handle: HANDLE) -> Option<u64> {
    // SAFETY: an all-zero `FILETIME` is valid; the call writes all four.
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { std::mem::zeroed() };
    // SAFETY: the handle is valid and every pointer is writable for the call.
    let done =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (done != 0).then_some(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
}

#[derive(Clone)]
struct Entry {
    pid: u32,
    parent: u32,
    name: String,
}

fn snapshot() -> Vec<Entry> {
    // SAFETY: the call returns a handle that this function closes, or an invalid one.
    let handle: HANDLE = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entries = Vec::new();
    // SAFETY: an all-zero entry is valid, and `dwSize` tells the call its layout.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: the handle is valid and the entry is writable for the call.
    let mut more = unsafe { Process32FirstW(handle, &mut entry) } != 0;
    while more {
        let name = name_of(&entry);
        entries.push(Entry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name,
        });
        // SAFETY: the same handle and entry as above.
        more = unsafe { Process32NextW(handle, &mut entry) } != 0;
    }
    // SAFETY: the handle came from the snapshot and is not used again.
    unsafe {
        CloseHandle(handle);
    }
    entries
}

fn name_of(entry: &PROCESSENTRY32W) -> String {
    let length = entry
        .szExeFile
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(entry.szExeFile.len());
    OsString::from_wide(&entry.szExeFile[..length])
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{is_console_process, subsystem_of};
    use std::path::PathBuf;

    #[test]
    fn the_subsystem_of_an_image_comes_from_its_header() {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let root = PathBuf::from(root).join("System32");
        assert_eq!(subsystem_of(&root.join("ping.exe")), Some(true));
        assert_eq!(subsystem_of(&root.join("notepad.exe")), Some(false));
        // An image that is not there is unknown, and the caller leaves it alone.
        assert_eq!(subsystem_of(&root.join("no-such-image.exe")), None);
        // The test binary itself is a console program.
        assert_eq!(is_console_process(std::process::id()), Some(true));
    }
}
