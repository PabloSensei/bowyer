//! System notifications: a session the user watches waits for them or finished while the
//! window is not in front. Windows shows a toast through the WinRT notifier from Windows
//! PowerShell (in the name of the installed shortcut's AppUserModelID when the installer
//! made one, otherwise in PowerShell's), Linux runs `notify-send`, macOS `osascript`. The
//! command runs in the background; a system without one shows nothing.

use std::process::{Command, Stdio};

/// The AppUserModelID of the shortcuts the installer makes (`packaging/packager.toml`).
#[cfg(windows)]
const APP_ID: &str = "dev.bowyer.app";
/// The AppUserModelID of Windows PowerShell: toasts in its name show without a shortcut.
#[cfg(windows)]
const POWERSHELL_ID: &str = r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// Shows a notification; errors are dropped, the window flashes anyway.
pub fn show(title: &str, body: &str) {
    log::info!("notification: {title}: {body}");
    if let Some(mut command) = command(title, body) {
        let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

/// Text for XML: the toast is an XML document.
#[cfg_attr(not(windows), allow(dead_code))]
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// A PowerShell string literal.
#[cfg_attr(not(windows), allow(dead_code))]
fn ps_literal(text: &str) -> String {
    crate::launch::ps_quoted(text)
}

/// The script that shows a toast in the name of `app_id`.
#[cfg_attr(not(windows), allow(dead_code))]
fn toast_script(app_id: &str, title: &str, body: &str) -> String {
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        xml_text(title),
        xml_text(body)
    );
    format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null\n\
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null\n\
         $xml = New-Object Windows.Data.Xml.Dom.XmlDocument\n\
         $xml.LoadXml({})\n\
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier({}).Show([Windows.UI.Notifications.ToastNotification]::new($xml))",
        ps_literal(&xml),
        ps_literal(app_id)
    )
}

/// The AppUserModelID toasts go out in: Bowyer's when its Start menu shortcut carries it
/// (Windows shows toasts only for an ID a shortcut registers), PowerShell's otherwise.
#[cfg(windows)]
fn app_id() -> &'static str {
    let shortcut = std::env::var_os("APPDATA")
        .map(|roaming| std::path::PathBuf::from(roaming).join(r"Microsoft\Windows\Start Menu\Programs\Bowyer\Bowyer.lnk"));
    let common = std::env::var_os("ProgramData")
        .map(|data| std::path::PathBuf::from(data).join(r"Microsoft\Windows\Start Menu\Programs\Bowyer\Bowyer.lnk"));
    if [shortcut, common].into_iter().flatten().any(|path| path.is_file()) { APP_ID } else { POWERSHELL_ID }
}

#[cfg(windows)]
fn command(title: &str, body: &str) -> Option<Command> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(envs::system_program("powershell.exe"));
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &crate::launch::powershell_encoded(&toast_script(app_id(), title, body))])
        .creation_flags(CREATE_NO_WINDOW);
    Some(command)
}

#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<Command> {
    let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    let mut command = Command::new("osascript");
    command.arg("-e").arg(format!("display notification {} with title {}", quote(body), quote(title)));
    Some(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut command = Command::new("notify-send");
    command.args(["--app-name=Bowyer", "--icon=bowyer", "--", title, body]);
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toast_escapes_its_text() {
        let script = toast_script("dev.bowyer.app", "Fix <it> & 'go'", "it's \"done\"");
        assert!(script.contains("<text>Fix &lt;it&gt; &amp; &apos;go&apos;</text>"), "{script}");
        assert!(script.contains("<text>it&apos;s &quot;done&quot;</text>"), "{script}");
        assert!(script.contains("CreateToastNotifier('dev.bowyer.app')"));
    }

    /// Shows a real notification: run by hand with `--ignored` and look.
    #[test]
    #[ignore]
    fn a_notification_shows() {
        show("Bowyer — test", "A notification from the test of notify.rs");
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
