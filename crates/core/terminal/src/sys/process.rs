use std::io;

use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

/// A child inherits ignoring Ctrl+C from its parent, and a program in the terminal then
/// never stops on it.
pub(crate) fn allow_ctrl_c_in_children() -> io::Result<()> {
    // SAFETY: a null handler is allowed and touches only this process's console state.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
