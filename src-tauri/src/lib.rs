use serde::Serialize;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, PhysicalPosition, WindowEvent,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use windows::Win32::{
    System::DataExchange::GetClipboardSequenceNumber,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_C, VK_CONTROL, VK_LWIN, VK_MENU,
            VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW},
    },
};

/// Two overlapping captures would race on the clipboard and could lose the user's copied text.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// What the popup receives: the selected text and the title of the window it came from.
#[derive(Clone, Serialize)]
struct Selection {
    text: String,
    window_title: String,
}

#[tauri::command]
fn api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().filter(|key| !key.is_empty())
}

fn foreground_window_title() -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// The user is still holding the shortcut keys; a Ctrl+C sent now would arrive as Win+Alt+Ctrl+C.
/// Returns false if they are still held after the deadline.
fn wait_for_modifiers_release() -> bool {
    let modifiers = [VK_LWIN, VK_RWIN, VK_MENU, VK_SHIFT, VK_CONTROL];
    let deadline = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < deadline {
        let any_down = modifiers
            .iter()
            .any(|key| unsafe { GetAsyncKeyState(key.0 as i32) } < 0);
        if !any_down {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn key_input(key: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                ..Default::default()
            },
        },
    }
}

fn send_ctrl_c() {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VK_C, false),
        key_input(VK_C, true),
        key_input(VK_CONTROL, true),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// Windows has no universal "read the selection" API, so we copy it with a simulated
/// Ctrl+C and then put the previous clipboard text back.
fn capture_selection() -> String {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return String::new();
    };
    let previous = clipboard.get_text().ok();
    let sequence_before = unsafe { GetClipboardSequenceNumber() };

    send_ctrl_c();

    let deadline = Instant::now() + Duration::from_millis(500);
    while unsafe { GetClipboardSequenceNumber() } == sequence_before {
        if Instant::now() > deadline {
            return String::new(); // Nothing was copied: no selection, clipboard untouched.
        }
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(30)); // Let the source app finish writing all formats.

    let text = clipboard.get_text().unwrap_or_default();
    if let Some(previous) = previous {
        let _ = clipboard.set_text(previous);
    }
    text
}

fn show_popup_near_cursor(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if let (Ok(cursor), Ok(size)) = (window.cursor_position(), window.outer_size()) {
        let mut x = cursor.x as i32 + 16;
        let mut y = cursor.y as i32 + 16;
        // Keep the popup inside the screen when the cursor is near an edge.
        if let Ok(Some(monitor)) = window.monitor_from_point(cursor.x, cursor.y) {
            let area = monitor.work_area();
            let right = area.position.x + area.size.width as i32 - size.width as i32;
            let bottom = area.position.y + area.size.height as i32 - size.height as i32;
            x = x.min(right).max(area.position.x);
            y = y.min(bottom).max(area.position.y);
        }
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
    let _ = window.show();
    let _ = window.set_focus();
}

fn explain_selection(app: &AppHandle) {
    let window_title = foreground_window_title();
    // Keys still held: don't inject anything; the popup opens empty so the user can type the term.
    let text = if wait_for_modifiers_release() {
        capture_selection()
    } else {
        String::new()
    };
    let _ = app.emit("explain", Selection { text, window_title });
    show_popup_near_cursor(app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::ALT), Code::KeyE);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, pressed, event| {
                    if pressed == &shortcut
                        && event.state() == ShortcutState::Pressed
                        && !CAPTURING.swap(true, Ordering::SeqCst)
                    {
                        let app = app.clone();
                        // Off the event loop: capturing the selection waits and sleeps.
                        thread::spawn(move || {
                            explain_selection(&app);
                            CAPTURING.store(false, Ordering::SeqCst);
                        });
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![api_key])
        .on_window_event(|window, event| {
            // Alt+F4 hides the popup instead of quitting; quit from the tray icon.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            app.global_shortcut().register(shortcut).map_err(|error| {
                format!("Could not register Win+Alt+E (is another app using it?): {error}")
            })?;

            let quit = MenuItem::with_id(app, "quit", "Salir", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Explicador: selecciona un texto y pulsa Win+Alt+E")
                .menu(&menu)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
