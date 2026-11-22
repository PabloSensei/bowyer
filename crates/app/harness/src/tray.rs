//! The icon of Bowyer in the notification area. A click shows the window (also when it was
//! hidden there on close), the menu shows it or quits.
//!
//! Windows: the icon lives on a thread of its own with a hidden window for its messages, and
//! comes back when Explorer restarts. Linux: a StatusNotifierItem with a dbusmenu menu on the
//! session bus, registered with the desktop's StatusNotifierWatcher (KDE, GNOME with the
//! AppIndicator extension, waybar, …); without a watcher there is no icon. There the window
//! is not hidden but closed, and the icon opens it again. macOS has no icon yet.

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

/// Whether the window is away in the icon: hidden (Windows) or closed (Linux). Nothing is
/// drawn then, so what would be said while drawing is said at once.
static WINDOW_AWAY: AtomicBool = AtomicBool::new(false);

pub fn window_away() -> bool {
    WINDOW_AWAY.load(Ordering::Relaxed)
}

pub fn set_window_away(away: bool) {
    WINDOW_AWAY.store(away, Ordering::Relaxed);
}

/// Whether the icon is in the notification area.
pub fn icon_up() -> bool {
    ICON_UP.load(Ordering::Relaxed)
}

/// Whether closing the window should hide it: asked for, and the icon is there to bring it back.
pub fn hides_on_close() -> bool {
    close_to_tray() && icon_up()
}

/// What the user asked of the icon that the window has to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Bring the window back, opening it again where closing took it away. (The icon of
    /// Windows shows the window itself.)
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Show,
    Quit,
}

/// The icon; [`Tray::remove`] takes it out of the notification area.
pub struct Tray {
    #[cfg(windows)]
    hwnd: usize,
    #[cfg(target_os = "linux")]
    connection: zbus::blocking::Connection,
}

/// The words of the icon, in the language of the window.
pub struct TrayText {
    pub tooltip: String,
    pub open: String,
    pub quit: String,
}

impl Tray {
    /// Puts the icon up for the window `main` (its `HWND` on Windows). `None` when the
    /// system has no notification area.
    pub fn start(main: Option<usize>, text: TrayText) -> Option<(Tray, UnboundedReceiver<TrayEvent>)> {
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
    set_window_away(true);
    imp::hide_window(main);
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::*;

    pub fn start(_main: Option<usize>, _text: TrayText, _sender: UnboundedSender<TrayEvent>) -> Option<Tray> {
        None
    }

    pub fn remove(_tray: &Tray) {}

    pub fn hide_window(_main: usize) {}
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use zbus::blocking::Connection;
    use zbus::blocking::connection::Builder;
    use zbus::interface;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, StructureBuilder, Value};

    use super::*;

    const WATCHER: &str = "org.kde.StatusNotifierWatcher";
    const ITEM_PATH: &str = "/StatusNotifierItem";
    const MENU_PATH: &str = "/MenuBar";
    const OPEN: i32 = 1;
    const SEPARATOR: i32 = 2;
    const QUIT: i32 = 3;

    /// The picture of the icon: ARGB32 in network byte order, at sizes hosts ask for.
    type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
    /// A dbusmenu item with its children: `(ia{sv}av)`.
    type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

    fn pixmaps() -> Pixmaps {
        let Ok(image) = image::load_from_memory(include_bytes!("../../../../assets/icon/bowyer-256.png")) else { return Vec::new() };
        [22u32, 32, 48, 64]
            .into_iter()
            .map(|side| {
                let rgba = image::imageops::resize(&image.to_rgba8(), side, side, image::imageops::FilterType::Lanczos3);
                let argb = rgba.pixels().flat_map(|pixel| [pixel[3], pixel[0], pixel[1], pixel[2]]).collect();
                (side as i32, side as i32, argb)
            })
            .collect()
    }

    struct Item {
        title: String,
        pixmaps: Pixmaps,
        sender: UnboundedSender<TrayEvent>,
    }

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl Item {
        #[zbus(property)]
        fn category(&self) -> &str {
            "ApplicationStatus"
        }

        #[zbus(property)]
        fn id(&self) -> &str {
            "bowyer"
        }

        #[zbus(property)]
        fn title(&self) -> &str {
            &self.title
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property)]
        fn window_id(&self) -> i32 {
            0
        }

        /// No name from the icon theme: an AppImage installs none, and some hosts show
        /// nothing rather than the picture when the name is not found.
        #[zbus(property)]
        fn icon_name(&self) -> &str {
            ""
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> Pixmaps {
            self.pixmaps.clone()
        }

        #[zbus(property)]
        fn tool_tip(&self) -> (String, Pixmaps, String, String) {
            (String::new(), Vec::new(), self.title.clone(), String::new())
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from(MENU_PATH).expect("a valid object path")
        }

        fn activate(&self, _x: i32, _y: i32) {
            let _ = self.sender.unbounded_send(TrayEvent::Show);
        }

        fn secondary_activate(&self, _x: i32, _y: i32) {
            let _ = self.sender.unbounded_send(TrayEvent::Show);
        }

        fn context_menu(&self, _x: i32, _y: i32) {}

        fn scroll(&self, _delta: i32, _orientation: &str) {}
    }

    /// The menu of the icon, as the dbusmenu protocol lays it out: a root (0) with the open
    /// item, a separator and the quit item.
    struct Menu {
        open: String,
        quit: String,
        sender: UnboundedSender<TrayEvent>,
    }

    impl Menu {
        fn properties(&self, id: i32) -> HashMap<String, OwnedValue> {
            let mut properties = HashMap::new();
            let mut put = |key: &str, value: Value<'_>| {
                if let Ok(value) = OwnedValue::try_from(value) {
                    properties.insert(key.to_owned(), value);
                }
            };
            match id {
                0 => put("children-display", Value::from("submenu")),
                OPEN => put("label", Value::from(self.open.as_str())),
                SEPARATOR => put("type", Value::from("separator")),
                QUIT => put("label", Value::from(self.quit.as_str())),
                _ => {}
            }
            properties
        }

        /// An item as a `(ia{sv}av)` structure, for the children of the root.
        fn node(&self, id: i32) -> Option<OwnedValue> {
            let structure: Structure<'_> = StructureBuilder::new()
                .add_field(id)
                .add_field(self.properties(id))
                .add_field(Vec::<OwnedValue>::new())
                .build()
                .ok()?;
            OwnedValue::try_from(Value::from(structure)).ok()
        }
    }

    #[interface(name = "com.canonical.dbusmenu")]
    impl Menu {
        #[zbus(property)]
        fn version(&self) -> u32 {
            3
        }

        #[zbus(property)]
        fn text_direction(&self) -> &str {
            "ltr"
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "normal"
        }

        #[zbus(property)]
        fn icon_theme_path(&self) -> Vec<String> {
            Vec::new()
        }

        fn get_layout(&self, parent_id: i32, _recursion_depth: i32, _property_names: Vec<String>) -> (u32, Layout) {
            let children = if parent_id == 0 { [OPEN, SEPARATOR, QUIT].into_iter().filter_map(|id| self.node(id)).collect() } else { Vec::new() };
            (1, (parent_id, self.properties(parent_id), children))
        }

        fn get_group_properties(&self, ids: Vec<i32>, _property_names: Vec<String>) -> Vec<(i32, HashMap<String, OwnedValue>)> {
            let ids = if ids.is_empty() { vec![0, OPEN, SEPARATOR, QUIT] } else { ids };
            ids.into_iter().map(|id| (id, self.properties(id))).collect()
        }

        fn get_property(&self, id: i32, name: &str) -> OwnedValue {
            self.properties(id).remove(name).unwrap_or_else(|| OwnedValue::from(0i32))
        }

        fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
            if event_id != "clicked" {
                return;
            }
            let event = match id {
                OPEN => TrayEvent::Show,
                QUIT => TrayEvent::Quit,
                _ => return,
            };
            let _ = self.sender.unbounded_send(event);
        }

        fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
            for (id, event_id, data, timestamp) in events {
                self.event(id, &event_id, data, timestamp);
            }
            Vec::new()
        }

        fn about_to_show(&self, _id: i32) -> bool {
            false
        }

        fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
            (Vec::new(), Vec::new())
        }
    }

    fn register(connection: &Connection, name: &str) -> zbus::Result<()> {
        connection.call_method(Some(WATCHER), "/StatusNotifierWatcher", Some(WATCHER), "RegisterStatusNotifierItem", &(name,)).map(|_| ())
    }

    pub fn start(_main: Option<usize>, text: TrayText, sender: UnboundedSender<TrayEvent>) -> Option<Tray> {
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let item = Item { title: text.tooltip, pixmaps: pixmaps(), sender: sender.clone() };
        let menu = Menu { open: text.open, quit: text.quit, sender };
        let built = Builder::session().and_then(|builder| builder.name(name.as_str())?.serve_at(ITEM_PATH, item)?.serve_at(MENU_PATH, menu)?.build());
        let connection = match built {
            Ok(connection) => connection,
            Err(error) => {
                log::info!("no session bus for the tray icon: {error}");
                return None;
            }
        };
        // No watcher: the desktop shows no such icons.
        if let Err(error) = register(&connection, &name) {
            log::info!("no StatusNotifierWatcher for the tray icon: {error}");
            return None;
        }
        // A watcher that starts again (a restarted panel) forgets the icon: register anew.
        let watch = connection.clone();
        let _ = std::thread::Builder::new().name("tray".into()).spawn(move || {
            let Ok(bus) = zbus::blocking::fdo::DBusProxy::new(&watch) else { return };
            let Ok(changes) = bus.receive_name_owner_changed() else { return };
            for change in changes {
                let Ok(args) = change.args() else { continue };
                if args.name().as_str() == WATCHER && args.new_owner().is_some() {
                    let _ = register(&watch, &name);
                }
            }
        });
        Some(Tray { connection })
    }

    pub fn remove(tray: &Tray) {
        let _ = tray.connection.release_name(format!("org.kde.StatusNotifierItem-{}-1", std::process::id()));
    }

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

    pub fn start(main: Option<usize>, text: TrayText, sender: UnboundedSender<TrayEvent>) -> Option<Tray> {
        let main = main?;
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
        set_window_away(false);
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
