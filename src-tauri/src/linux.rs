//! What Linux does differently from Windows, apart from the window's context (`screen_context`).

use gtk::prelude::*;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

/// Wayland lets no app register a global shortcut or read the selection in the background.
pub fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// The PRIMARY selection: the last text selected with the mouse, with no Ctrl+C to fake. On X11
/// it is always readable; on Wayland only where the desktop has the data-control protocol (KDE,
/// Sway...), not on GNOME. It may be an older selection: `current_selection` checks it.
pub fn primary_selection() -> Option<String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    arboard::Clipboard::new().ok()?.get().clipboard(LinuxClipboardKind::Primary).text().ok()
}

/// What the app says is selected, read through the accessibility bus.
#[derive(Debug, PartialEq)]
pub enum Selected {
    Text(String),
    /// The app has a text object and says nothing in it is selected.
    Nothing,
    /// The app published no text object (or is not on the bus): it cannot say.
    Unknown,
}

/// What to explain. The accessibility bus knows what is selected right now, but on a web page it
/// may give only the part inside one paragraph; PRIMARY has it all but may be a selection the user
/// has since cleared (a token selected in a terminal an hour ago). PRIMARY wins when it contains
/// what the bus saw, and alone only when the app cannot tell the bus anything.
pub fn current_selection(seen: Selected, primary: Option<String>) -> String {
    // Apps and PRIMARY may break lines and space words differently.
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    match (seen, primary) {
        (Selected::Text(seen), Some(primary)) if words(&primary).contains(&words(&seen)) => primary,
        (Selected::Text(seen), _) => seen,
        (Selected::Nothing, _) => String::new(),
        (Selected::Unknown, primary) => primary.unwrap_or_default(),
    }
}

/// Apps opened from the desktop get a short PATH, without what the shell adds in ~/.bashrc or
/// ~/.profile (nvm, Volta, ~/.local/bin...): `claude` and `codex` would not be found. Take the PATH
/// that the user's own shell has in a terminal.
pub fn use_login_path() {
    const MARK: &str = "__XPLAINER_PATH__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut command = Command::new(shell);
    // Marks on both sides: the startup files may print before it, and ~/.bash_logout after.
    command.args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")]);
    // A shell whose startup files hang must not stop the app from opening.
    let Some(output) = crate::status_output(command, Duration::from_secs(3)) else { return };
    let printed = String::from_utf8_lossy(&output.stdout);
    let Some((path, _)) = printed.rsplit_once(MARK) else { return };
    let Some((_, path)) = path.rsplit_once(MARK) else { return };
    let path = path.trim();
    if !path.is_empty() {
        // Safe: called first thing in `run`, before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
    }
}

pub fn total_memory_gb() -> u64 {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .unwrap_or(0);
    kib / (1024 * 1024)
}

/// The name the single-instance plugin holds on the session bus: the app's identifier plus
/// ".SingleInstance".
pub const SINGLE_INSTANCE_NAME: &str = "com.kiook.xplainer.SingleInstance";

/// Whether another copy of Xplainer is running. One quick call to the session bus.
pub fn already_running() -> bool {
    let Ok(session) = zbus::blocking::Connection::session() else { return false };
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&session) else { return false };
    let Ok(name) = zbus::names::BusName::try_from(SINGLE_INSTANCE_NAME) else { return false };
    dbus.name_has_owner(name).unwrap_or(false)
}

/// Where `program` is on the PATH, looked up as the shell does (no `which`, which minimal
/// systems may lack).
pub fn find_program(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|folder| folder.join(program))
        .find(|file| fs::metadata(file).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0))
}

/// The `ollama` program, which its Linux installer puts on the PATH.
pub fn ollama_app_path() -> Option<PathBuf> {
    find_program("ollama")
}

/// Ollama's installer runs it as a system service that starts with the computer. When it is not
/// running, start the server as the user would in a terminal.
pub fn start_ollama() -> Result<(), String> {
    let ollama = ollama_app_path().ok_or("No encuentro Ollama. Instálalo desde ollama.com.")?;
    crate::background(&mut Command::new(ollama))
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("No se pudo abrir Ollama: {error}"))
}

/// The CLI's own login in a terminal window, which stays open only if it fails. The command is
/// one of two fixed lines, never text from outside, so it can go through `sh`.
pub fn open_login(login: &[&str]) -> Result<(), String> {
    let line = login.join(" ");
    let script = format!("{line} || {{ echo; printf 'Pulsa Intro para cerrar. '; read _; }}");
    // The desktop's default terminal first (Debian and Ubuntu), then the usual ones: GNOME's
    // (Ptyxis in recent releases, GNOME Terminal and Console before), KDE's, Xfce's and others.
    let terminals: [&[&str]; 10] = [
        &["x-terminal-emulator", "-e"],
        &["ptyxis", "--"],
        &["gnome-terminal", "--"],
        &["kgx", "--"],
        &["konsole", "-e"],
        &["xfce4-terminal", "-x"],
        &["kitty"],
        &["alacritty", "-e"],
        &["foot"],
        &["xterm", "-e"],
    ];
    for terminal in terminals {
        let started = Command::new(terminal[0]).args(&terminal[1..]).args(["sh", "-c", &script]).spawn();
        if started.is_ok() {
            return Ok(());
        }
    }
    Err(format!("No encuentro una terminal. Abre una y ejecuta: {line}"))
}

/// The app has no terminal when opened from the desktop, so errors go to a window too. GTK
/// calls must run on the main thread, where `setup` and the tray menu run.
pub fn show_error(message: &str) {
    eprintln!("Xplainer: {message}");
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Error,
        gtk::ButtonsType::Ok,
        message,
    );
    dialog.set_title("Xplainer");
    dialog.run();
    dialog.close();
}
