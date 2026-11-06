use serde::Serialize;
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    ipc::Channel,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, PhysicalPosition, State, WindowEvent,
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

/// Hides the console window that Windows would open for the child process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Tracks the `claude -p` request in flight. Request ids only grow, so cancelling one also
/// covers every older one: a cancel that arrives before its request is registered still counts.
#[derive(Default)]
struct ClaudeCode(Mutex<Requests>);

#[derive(Default)]
struct Requests {
    cancelled_up_to: u64,
    running: Option<(u64, Option<Child>)>,
}

impl Requests {
    fn is_current(&self, request_id: u64) -> bool {
        self.running.as_ref().is_some_and(|(id, _)| *id == request_id)
    }

    fn is_live(&self, request_id: u64) -> bool {
        request_id > self.cancelled_up_to && self.is_current(request_id)
    }

    fn kill_running(&mut self) {
        if let Some((_, Some(child))) = self.running.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ClaudeCode {
    /// Tracks a new request and stops the previous one: one answer at a time. Fails if the
    /// request was already cancelled or a newer one has started.
    fn begin(&self, request_id: u64) -> Result<(), String> {
        let mut requests = self.0.lock().unwrap();
        let newer_started = requests.running.as_ref().is_some_and(|(id, _)| *id > request_id);
        if request_id <= requests.cancelled_up_to || newer_started {
            return Err("cancelled".into());
        }
        requests.kill_running();
        requests.running = Some((request_id, None));
        Ok(())
    }

    /// Hands the started process to its request; kills it if the request was cancelled meanwhile.
    fn attach(&self, request_id: u64, mut child: Child) -> Result<(), String> {
        let mut requests = self.0.lock().unwrap();
        if requests.is_live(request_id) {
            requests.running = Some((request_id, Some(child)));
            Ok(())
        } else {
            let _ = child.kill();
            let _ = child.wait();
            Err("cancelled".into())
        }
    }

    /// Cancels `request_id` and every older request, even one that has not started yet.
    fn cancel(&self, request_id: u64) {
        let mut requests = self.0.lock().unwrap();
        requests.cancelled_up_to = requests.cancelled_up_to.max(request_id);
        if requests.running.as_ref().is_some_and(|(id, _)| *id <= request_id) {
            requests.kill_running();
        }
    }

    /// Stops tracking the request; returns its process unless it was cancelled.
    fn finish(&self, request_id: u64) -> Option<Child> {
        let mut requests = self.0.lock().unwrap();
        if !requests.is_current(request_id) {
            return None; // A newer request took over: leave it alone.
        }
        let live = requests.is_live(request_id);
        let (_, child) = requests.running.take()?;
        if live { child } else { None }
    }
}

#[tauri::command]
fn cancel_claude_code(claude: State<'_, ClaudeCode>, request_id: u64) {
    claude.cancel(request_id);
}

/// Answers with the user's Claude subscription instead of the API: runs Claude Code
/// headless, streams the text through `on_text` and returns the full answer.
#[tauri::command]
async fn ask_claude_code(
    app: AppHandle,
    request_id: u64,
    model: String,
    effort: String,
    system: String,
    prompt: String,
    on_text: Channel<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let claude = app.state::<ClaudeCode>();
        run_claude_code(claude.inner(), request_id, &model, &effort, &system, &prompt, &on_text)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn run_claude_code(
    claude: &ClaudeCode,
    request_id: u64,
    model: &str,
    effort: &str,
    system: &str,
    prompt: &str,
    on_text: &Channel<String>,
) -> Result<String, String> {
    claude.begin(request_id)?;

    let spawned = Command::new("claude")
        .args([
            "-p", "--model", model, "--effort", effort, "--system-prompt", system,
            "--output-format", "stream-json", "--include-partial-messages", "--verbose",
            // A plain question-answer call: no tools, MCP servers, skills or saved session.
            // No setting sources at all: with "project", Claude Code would walk up from the
            // working directory and load the user's own ~/.claude/CLAUDE.md as instructions.
            "--tools", "", "--strict-mcp-config", "--disable-slash-commands",
            "--setting-sources", "", "--no-session-persistence",
        ])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            claude.finish(request_id);
            return Err(format!(
                "No se pudo ejecutar Claude Code (`claude`). Instálalo con el instalador nativo e inicia sesión. {error}"
            ));
        }
    };

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    claude.attach(request_id, child)?;

    // The prompt goes through stdin: no command-line length limit or quoting issues.
    let _ = stdin.write_all(prompt.as_bytes());
    drop(stdin);
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut answer = None;
    let mut failure = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("stream_event") => {
                let delta = &event["event"]["delta"];
                if delta["type"] == "text_delta" {
                    if let Some(text) = delta["text"].as_str() {
                        let _ = on_text.send(text.to_string());
                    }
                }
            }
            Some("result") => {
                let text = event["result"].as_str().unwrap_or_default().to_string();
                if event["is_error"] == true {
                    // Some error results carry an `errors` list instead of `result`.
                    let errors = event["errors"].as_array().map(|errors| {
                        errors.iter().filter_map(|error| error.as_str()).collect::<Vec<_>>().join("; ")
                    });
                    failure = Some(if text.is_empty() { errors.unwrap_or_default() } else { text });
                } else {
                    answer = Some(text);
                }
            }
            _ => {}
        }
    }

    let Some(mut child) = claude.finish(request_id) else {
        return Err("cancelled".into());
    };
    let status = child.wait().map_err(|error| error.to_string())?;
    let stderr = stderr_reader.join().unwrap_or_default();
    match (answer, failure) {
        (_, Some(message)) if !message.is_empty() => Err(format!("Claude Code: {message}")),
        (Some(text), None) if status.success() && !text.trim().is_empty() => Ok(text),
        _ => Err(format!("Claude Code no respondió. {}", stderr.trim())),
    }
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
        .manage(ClaudeCode::default())
        .invoke_handler(tauri::generate_handler![api_key, ask_claude_code, cancel_claude_code])
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
