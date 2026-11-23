//! Bowyer: one window over every AI coding harness on this computer and on hosts reached
//! over SSH. `bowyer [--data-dir DIR]`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use library::Library;
use ui_kit::prelude::*;
use ui_kit::*;

mod display;
mod extensions;
mod highlight;
mod rerank;
mod i18n;
mod launch;
mod markdown;
mod notify;
mod tray;
mod shell;

use shell::{APP_NAME, Shell};

/// Writes the log of the window and of GPUI to standard error when `BOWYER_LOG` names a
/// level (`error`, `warn`, `info`, `debug`, `trace`), for a start that goes wrong.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
    }

    fn flush(&self) {}
}

fn init_log() {
    let Ok(level) = std::env::var("BOWYER_LOG") else { return };
    let Ok(level) = level.parse::<log::LevelFilter>() else { return };
    if log::set_logger(&StderrLog).is_ok() {
        log::set_max_level(level);
    }
}

/// Where the application keeps its database, themes and caches.
fn data_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-dir"
            && let Some(dir) = args.next()
        {
            return PathBuf::from(dir);
        }
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    };
    let base = base.unwrap_or_else(std::env::temp_dir);
    let dir = base.join(APP_NAME);
    // The working title was Quiver: its folder moves over once.
    let old = base.join("Quiver");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    dir
}

/// The AppUserModelID of the window, the identifier of the packages
/// (`packaging/packager.toml`): the installer gives it to the Start menu and desktop
/// shortcuts, so the taskbar keeps the window under a pinned shortcut.
#[cfg(windows)]
fn set_app_id() {
    let id: Vec<u16> = "dev.bowyer.app".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated wide string that lives across the call.
    unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
    }
}

/// Opens the main window on `view`, at the start and when the icon brings a closed window
/// back. Returns its native handle where the icon needs one.
fn open_main_window(view: Entity<Shell>, cx: &mut App) -> Option<usize> {
    let bounds = Bounds::centered(None, size(px(1480.), px(920.)), cx);
    let found = std::rc::Rc::new(std::cell::Cell::new(None));
    let handle = found.clone();
    let asked = view.clone();
    let opened = cx.open_window(shell::window_options(bounds), move |window, cx| {
        // The root takes the keys from the start, so Ctrl+K works before any click.
        let focus = view.read(cx).focus.clone();
        focus.focus(window, cx);
        window.activate_window();
        let hwnd = tray::native_handle(window);
        handle.set(hwnd);
        window.on_window_should_close(cx, move |_, cx| match hwnd {
            Some(hwnd) if tray::hides_on_close() => {
                tray::hide_window(hwnd);
                false
            }
            // Running turns and terminals: the window asks, and its answer quits.
            _ => asked.update(cx, |shell, cx| shell.may_quit(cx)),
        });
        view
    });
    match opened {
        Ok(_) => tray::set_window_away(false),
        Err(error) => eprintln!("cannot open the window: {error}"),
    }
    found.get()
}

/// Puts up the icon of the window and quits when its menu says so; takes it down on quit.
fn start_tray(hwnd: Option<usize>, view: Entity<Shell>, cx: &mut App) {
    use futures::StreamExt;
    let text = tray::TrayText { tooltip: shell::APP_NAME.to_owned(), open: i18n::t("tray.open").to_owned(), quit: i18n::t("tray.quit").to_owned() };
    let Some((icon, mut events)) = tray::Tray::start(hwnd, text) else { return };
    // Where the window closes into the icon rather than hiding, Bowyer outlives its last
    // window then, and quits with it otherwise.
    if cfg!(target_os = "linux") {
        cx.set_quit_mode(QuitMode::Explicit);
        cx.on_window_closed(|cx, _| {
            if !cx.windows().is_empty() {
                return;
            }
            if tray::hides_on_close() {
                tray::set_window_away(true);
            } else {
                cx.quit();
            }
        })
        .detach();
    }
    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            match event {
                tray::TrayEvent::Quit => cx.update(|cx| {
                    if view.update(cx, |shell, cx| shell.may_quit(cx)) {
                        cx.quit();
                        return;
                    }
                    // The question is in the window: bring it up.
                    match cx.windows().first() {
                        Some(window) => {
                            tray::set_window_away(false);
                            let _ = window.update(cx, |_, window, _| window.activate_window());
                        }
                        None => {
                            open_main_window(view.clone(), cx);
                        }
                    }
                }),
                tray::TrayEvent::Show => cx.update(|cx| match cx.windows().first() {
                    Some(window) => {
                        let _ = window.update(cx, |_, window, _| window.activate_window());
                    }
                    None => {
                        open_main_window(view.clone(), cx);
                    }
                }),
            }
        }
    })
    .detach();
    cx.on_app_quit(move |_| {
        icon.remove();
        async {}
    })
    .detach();
}

/// `ssh` started Bowyer as its askpass program: the question is the only argument, and the
/// answer is the kept password of the host on standard output, or nothing and a failure.
fn answer_askpass() -> ! {
    use std::io::Write;
    let host = std::env::var(envs::ASKPASS_HOST).unwrap_or_default();
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match envs::askpass_answer(&host, &prompt) {
        Some(password) => {
            let mut out = std::io::stdout().lock();
            let written = writeln!(out, "{password}").and_then(|()| out.flush());
            std::process::exit(i32::from(written.is_err()));
        }
        None => std::process::exit(1),
    }
}

fn main() {
    if std::env::var_os(envs::ASKPASS_HOST).is_some() && std::env::args().len() == 2 {
        answer_askpass();
    }
    init_log();
    #[cfg(windows)]
    set_app_id();
    let data = data_dir();
    // `ssh` asks this program for the passwords of password hosts, and reads the host keys
    // the user trusted in the window from the data folder.
    if let Ok(exe) = std::env::current_exe() {
        envs::set_askpass(exe);
    }
    envs::set_known_hosts(data.join("known_hosts"));
    let library = match Library::open(&data.join("library.db")) {
        Ok(library) => library,
        Err(error) => {
            eprintln!("cannot open the library in {}: {error}", data.display());
            std::process::exit(1);
        }
    };
    // `bowyer mcp`: the memory server for harnesses, on standard input and output, no window.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        let project = std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default();
        let server = mcp::Server::new(library, project, data.join("cache"));
        let result = server.serve(std::io::stdin().lock(), std::io::stdout().lock());
        std::process::exit(i32::from(result.is_err()));
    }
    let lang = library.setting("language").and_then(|id| i18n::Lang::from_id(&id)).unwrap_or_else(i18n::Lang::system);
    i18n::set_lang(lang);
    let theme_id = library.setting("theme");
    let close_to_tray = library.setting("tray.close").as_deref() == Some("true");

    ui_kit::app().run(move |cx| {
        ui_kit::init(true, cx);
        let catalog = ThemeCatalog::load(&data);
        if let Some(id) = &theme_id {
            cx.set_global(catalog.theme(id));
        }
        let cache = data.join("cache");
        let settings = data.clone();
        tray::set_close_to_tray(close_to_tray);
        let view = cx.new(|cx| Shell::new(library, settings, cache, cx));
        let hwnd = open_main_window(view.clone(), cx);
        if cx.windows().is_empty() {
            std::process::exit(1);
        }
        start_tray(hwnd, view, cx);
        cx.activate(true);
    });
}
