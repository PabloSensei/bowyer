//! The icon of Bowyer in the notification area of Windows. A click shows the window (also
//! when it was hidden there on close), the menu shows it or quits. The icon lives on a thread
//! of its own with a hidden window for its messages, and comes back when Explorer restarts.
//! Other systems have no icon yet.

use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

/// Whether closing the window hides it next to the icon instead of quitting.
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);
/// Whether the icon is up, so a hidden window can come back.
static ICON_UP: AtomicBool = AtomicBool::new(false);

pub fn set_close_to_tray(on: bool) {
    CLOSE_TO_TRAY.store(on, Ordering::Relaxed);
}

pub fn close_to_tray() -> bool {
    CLOSE_TO_TRAY.load(Ordering::Relaxed)
}

/// Whether closing the window should hide it: asked for, and the icon is there to bring it back.
pub fn hides_on_close() -> bool {
    close_to_tray() && ICON_UP.load(Ordering::Relaxed)
}

/// What the user asked of the icon that the window has to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Quit,
}

/// The icon; [`Tray::remove`] takes it out of the notification area.
pub struct Tray {
    #[cfg(windows)]
    hwnd: usize,
}

/// The words of the icon, in the language of the window.
pub struct TrayText {
    pub tooltip: String,
    pub open: String,
    pub quit: String,
}

impl Tray {
    /// Puts the icon up for the window `main` (its `HWND`). `None` when the system has none.
    pub fn start(main: usize, text: TrayText) -> Option<(Tray, UnboundedReceiver<TrayEvent>)> {
        let (sender, receiver) = unbounded();
        let tray = imp::start(main, text, sender)?;
        ICON_UP.store(true, Ordering::Relaxed);
        Some((tray, receiver))
    }

    pub fn remove(&self) {
        ICON_UP.store(false, Ordering::Relaxed);
        imp::remove(self);
    }
}

/// The `HWND` of a window on Windows.
pub fn native_handle(window: &ui_kit::Window) -> Option<usize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
        _ => None,
    }
}

/// Closes the window, or hides it next to the icon when the user asked for that.
pub fn close_window(window: &mut ui_kit::Window) {
    match native_handle(window) {
        Some(hwnd) if hides_on_close() => hide_window(hwnd),
        _ => window.remove_window(),
    }
}

/// Hides the window `main` (its `HWND`): it stays reachable from the icon.
pub fn hide_window(main: usize) {
    imp::hide_window(main);
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn start(_main: usize, _text: TrayText, _sender: UnboundedSender<TrayEvent>) -> Option<Tray> {
        None
    }

    pub fn remove(_tray: &Tray) {}

    pub fn hide_window(_main: usize) {}
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::sync::mpsc;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
        GetCursorPos, GetMessageW, GetSystemMetrics, IDI_APPLICATION, IMAGE_ICON, IsIconic, IsWindowVisible, LR_DEFAULTCOLOR,
        LoadIconW, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
        SM_CXSMICON, SM_CYSMICON, SW_RESTORE, SW_SHOW, SendMessageW, SetForegroundWindow, ShowWindow, TPM_BOTTOMALIGN,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
        WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, SW_HIDE,
    };

    use super::*;

    const CALLBACK: u32 = WM_APP + 1;
    const ICON_ID: u32 = 1;
    const OPEN: usize = 1;
    const QUIT: usize = 2;

    struct State {
        main: HWND,
        text: TrayText,
        sender: UnboundedSender<TrayEvent>,
        taskbar_created: u32,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    pub fn start(main: usize, text: TrayText, sender: UnboundedSender<TrayEvent>) -> Option<Tray> {
        let (ready, hwnd) = mpsc::channel::<Option<usize>>();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                // SAFETY: plain Win32 calls on this thread with buffers that outlive them.
                unsafe {
                    let instance = GetModuleHandleW(std::ptr::null());
                    let class = wide("BowyerTray");
                    let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance, lpszClassName: class.as_ptr(), ..std::mem::zeroed() };
                    RegisterClassW(&wc);
                    let title = wide("Bowyer tray");
                    // A top-level window that is never shown: message-only windows miss the
                    // broadcast of a restarted Explorer.
                    let window = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null());
                    if window.is_null() {
                        let _ = ready.send(None);
                        return;
                    }
                    let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
                    STATE.with(|state| *state.borrow_mut() = Some(State { main: main as HWND, text, sender, taskbar_created }));
                    let added = add_icon(window);
                    let _ = ready.send(added.then_some(window as usize));
                    if !added {
                        DestroyWindow(window);
                        return;
                    }
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .ok()?;
        hwnd.recv().ok().flatten().map(|hwnd| Tray { hwnd })
    }

    pub fn remove(tray: &Tray) {
        // SAFETY: the tray window handles WM_CLOSE by taking the icon away; a stale handle
        // only fails the call.
        unsafe { SendMessageW(tray.hwnd as HWND, WM_CLOSE, 0, 0) };
    }

    pub fn hide_window(main: usize) {
        // SAFETY: a window handle of this process.
        unsafe { ShowWindow(main as HWND, SW_HIDE) };
    }

    /// The small icon of `bowyer.exe` (resource 1, see build.rs), or the default one.
    unsafe fn icon() -> *mut c_void {
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let (width, height) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
            let own = LoadImageW(instance, 1 as _, IMAGE_ICON, width, height, LR_DEFAULTCOLOR);
            if own.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { own }
        }
    }

    fn icon_data(window: HWND) -> NOTIFYICONDATAW {
        // SAFETY: an all-zero NOTIFYICONDATAW is a valid empty value.
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = window;
        data.uID = ICON_ID;
        data
    }

    unsafe fn add_icon(window: HWND) -> bool {
        let mut data = icon_data(window);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = CALLBACK;
        data.hIcon = unsafe { icon() };
        let tip = STATE.with(|state| state.borrow().as_ref().map(|s| s.text.tooltip.clone()).unwrap_or_default());
        for (slot, unit) in data.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
            *slot = unit;
        }
        // SAFETY: `data` is filled for this window.
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
    }

    /// Brings the window back: shown, restored and in front.
    unsafe fn show_main(main: HWND) {
        unsafe {
            if IsWindowVisible(main) == 0 {
                ShowWindow(main, SW_SHOW);
            }
            if IsIconic(main) != 0 {
                ShowWindow(main, SW_RESTORE);
            }
            SetForegroundWindow(main);
        }
    }

    unsafe fn show_menu(window: HWND) -> usize {
        let (open, quit) = STATE.with(|state| {
            state.borrow().as_ref().map(|s| (s.text.open.clone(), s.text.quit.clone())).unwrap_or_default()
        });
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return 0;
            }
            AppendMenuW(menu, MF_STRING, OPEN, wide(&open).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, QUIT, wide(&quit).as_ptr());
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            // The menu closes on a click elsewhere only when its window is in front.
            SetForegroundWindow(window);
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, point.x, point.y, 0, window, std::ptr::null());
            DestroyMenu(menu);
            chosen as usize
        }
    }

    unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let main = STATE.with(|state| state.borrow().as_ref().map(|s| (s.main, s.taskbar_created)));
        let Some((main, taskbar_created)) = main else {
            return unsafe { DefWindowProcW(window, message, wparam, lparam) };
        };
        unsafe {
            match message {
                CALLBACK => {
                    match (lparam & 0xFFFF) as u32 {
                        WM_LBUTTONUP => show_main(main),
                        WM_RBUTTONUP | WM_CONTEXTMENU => match show_menu(window) {
                            OPEN => show_main(main),
                            QUIT => STATE.with(|state| {
                                if let Some(state) = state.borrow().as_ref() {
                                    let _ = state.sender.unbounded_send(TrayEvent::Quit);
                                }
                            }),
                            _ => {}
                        },
                        _ => {}
                    }
                    0
                }
                WM_CLOSE => {
                    Shell_NotifyIconW(NIM_DELETE, &icon_data(window));
                    DestroyWindow(window);
                    0
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    0
                }
                _ if message == taskbar_created && taskbar_created != 0 => {
                    add_icon(window);
                    0
                }
                _ => DefWindowProcW(window, message, wparam, lparam),
            }
        }
    }
}
