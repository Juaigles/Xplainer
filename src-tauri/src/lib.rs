use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, PhysicalPosition, Runtime, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use windows::{
    core::{w, HSTRING},
    Win32::{
        System::DataExchange::GetClipboardSequenceNumber,
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
                KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_C, VK_CONTROL, VK_LWIN,
                VK_MENU, VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::{
                GetForegroundWindow, GetWindowTextW, MessageBoxW, MB_ICONERROR, MB_OK,
            },
        },
    },
};

const SHORTCUT_NAME: &str = "Win+Shift+X";

/// Two overlapping captures would race on the clipboard and could lose the user's copied text.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// What the popup receives: the selected text and the title of the window it came from.
#[derive(Clone, Serialize)]
struct Selection {
    text: String,
    window_title: String,
}

/// Hides the console window that Windows would open for a child process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Tracks the answer in flight, whatever model gives it. Request ids only grow, so cancelling
/// one also covers every older one: a cancel that arrives before its request starts still counts.
#[derive(Default)]
struct Answers(Mutex<Requests>);

#[derive(Default)]
struct Requests {
    cancelled_up_to: u64,
    /// The request in flight and, for the CLIs, its process.
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
            kill_tree(child);
        }
    }
}

/// `codex` may start through a .cmd launcher (npm, Volta), so killing only our child would
/// leave the real process running: kill the whole process tree.
fn kill_tree(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

impl Answers {
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
            kill_tree(&mut child);
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

    fn is_live(&self, request_id: u64) -> bool {
        self.0.lock().unwrap().is_live(request_id)
    }

    /// Stops tracking the request. Fails if it was cancelled or replaced; otherwise returns
    /// its process, if it has one.
    fn finish(&self, request_id: u64) -> Result<Option<Child>, String> {
        let mut requests = self.0.lock().unwrap();
        if !requests.is_live(request_id) {
            if requests.is_current(request_id) {
                requests.running = None; // Cancelled: its process is already dead.
            }
            return Err("cancelled".into());
        }
        Ok(requests.running.take().and_then(|(_, child)| child))
    }
}

#[tauri::command]
async fn cancel_answer(app: AppHandle, request_id: u64) -> Result<(), String> {
    // Off the UI thread: killing a process tree waits for `taskkill`.
    blocking(app, move |answers| {
        answers.cancel(request_id);
        Ok(())
    })
    .await
}

/// Runs blocking work (processes, HTTP) off the async runtime.
async fn blocking<T: Send + 'static>(
    app: AppHandle,
    work: impl FnOnce(&Answers) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || work(app.state::<Answers>().inner()))
        .await
        .map_err(|error| error.to_string())?
}

/// Values that end up on a command line must be plain words (`cmd` would interpret `&`, `|`...).
fn plain_word(value: &str) -> Result<&str, String> {
    let plain = !value.is_empty()
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
    if plain { Ok(value) } else { Err(format!("Valor no válido: {value}")) }
}

#[derive(Default)]
struct Outcome {
    answer: Option<String>,
    failure: Option<String>,
    /// An error the CLI may still recover from (e.g. a reconnect); shown only if no answer arrives.
    last_error: Option<String>,
}

/// Runs a CLI that prints JSON events on stdout. `on_event` streams text and records the outcome.
fn run_cli(
    answers: &Answers,
    request_id: u64,
    mut command: Command,
    prompt: &str,
    name: &str,
    mut on_event: impl FnMut(&Value, &mut Outcome),
) -> Result<String, String> {
    answers.begin(request_id)?;
    let spawned = command
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let _ = answers.finish(request_id);
            return Err(format!(
                "No se pudo ejecutar {name}: {error}. Comprueba que está instalado y con la sesión iniciada."
            ));
        }
    };

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    answers.attach(request_id, child)?;

    // The prompt goes through stdin: no command-line length limit or quoting issues.
    let _ = stdin.write_all(prompt.as_bytes());
    drop(stdin);
    let stderr_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut outcome = Outcome::default();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Ok(event) = serde_json::from_str::<Value>(&line) {
            on_event(&event, &mut outcome);
        }
    }

    let mut child = answers.finish(request_id)?.ok_or_else(|| "cancelled".to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;
    let stderr = stderr_reader.join().unwrap_or_default();
    let Outcome { answer, failure, last_error } = outcome;
    match (answer, failure) {
        (_, Some(message)) if !message.is_empty() => Err(format!("{name}: {message}")),
        (Some(text), None) if status.success() && !text.trim().is_empty() => Ok(text),
        _ => Err(format!("{name} no respondió. {}", last_error.unwrap_or(stderr).trim())),
    }
}

/// Claude with the user's subscription: Claude Code headless, streaming the text.
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
    blocking(app, move |answers| {
        let mut command = Command::new("claude");
        command.args([
            "-p", "--model", &model, "--effort", &effort, "--system-prompt", &system,
            "--output-format", "stream-json", "--include-partial-messages", "--verbose",
            // A plain question-answer call: no tools, MCP servers, skills or saved session.
            // No setting sources at all: with "project", Claude Code would walk up from the
            // working directory and load the user's own ~/.claude/CLAUDE.md as instructions.
            "--tools", "", "--strict-mcp-config", "--disable-slash-commands",
            "--setting-sources", "", "--no-session-persistence",
        ]);
        // Use the subscription login: with these set, `claude -p` would bill the API instead.
        command.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
        run_cli(answers, request_id, command, &prompt, "Claude Code", |event, outcome| {
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
                        outcome.failure = Some(if text.is_empty() { errors.unwrap_or_default() } else { text });
                    } else {
                        outcome.answer = Some(text);
                    }
                }
                _ => {}
            }
        })
    })
    .await
}

/// ChatGPT models with the user's subscription: Codex headless. Codex sends no partial text,
/// only the final answer.
#[tauri::command]
async fn ask_codex(
    app: AppHandle,
    request_id: u64,
    model: String,
    effort: String,
    prompt: String,
    on_text: Channel<String>,
) -> Result<String, String> {
    blocking(app, move |answers| {
        let effort_setting = format!("model_reasoning_effort={}", plain_word(&effort)?);
        // Through `cmd` so it also finds a `codex.cmd` launcher. Every argument is a plain word.
        let mut command = Command::new("cmd");
        command.args([
            "/d", "/c", "codex", "exec", "--json", "--ephemeral", "--skip-git-repo-check",
            // A plain question-answer call: no user config or rules, project instructions,
            // shell, app connectors, writes or web. The selected text is untrusted input.
            "--ignore-user-config", "--ignore-rules", "-s", "read-only",
            "--disable", "shell_tool", "--disable", "apps", "-c", "project_doc_max_bytes=0",
            "-c", "web_search=disabled", "-c", &effort_setting, "-m", plain_word(&model)?, "-",
        ]);
        // Use the ChatGPT login: with this set, Codex would bill the API instead.
        command.env_remove("CODEX_API_KEY");
        run_cli(answers, request_id, command, &prompt, "Codex", |event, outcome| {
            on_codex_event(event, outcome, &on_text)
        })
    })
    .await
}

fn on_codex_event(event: &Value, outcome: &mut Outcome, on_text: &Channel<String>) {
    match event["type"].as_str() {
        Some("item.completed") if event["item"]["type"] == "agent_message" => {
            let text = event["item"]["text"].as_str().unwrap_or_default().to_string();
            let _ = on_text.send(text.clone());
            outcome.answer = Some(text);
        }
        Some("turn.failed") => outcome.failure = event["error"]["message"].as_str().map(String::from),
        // Codex also reports errors it retries ("Reconnecting... 1/5"); they are not fatal.
        Some("error") => outcome.last_error = event["message"].as_str().map(String::from),
        _ => {}
    }
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Free models served by Ollama (local ones and Ollama's cloud ones), streaming the text.
#[tauri::command]
async fn ask_ollama(
    app: AppHandle,
    request_id: u64,
    model: String,
    system: String,
    messages: Vec<ChatMessage>,
    on_text: Channel<String>,
) -> Result<String, String> {
    blocking(app, move |answers| {
        answers.begin(request_id)?;
        let answer = run_ollama(answers, request_id, &model, &system, &messages, &on_text);
        answers.finish(request_id).and(answer) // A cancel wins over whatever arrived.
    })
    .await
}

fn run_ollama(
    answers: &Answers,
    request_id: u64,
    model: &str,
    system: &str,
    messages: &[ChatMessage],
    on_text: &Channel<String>,
) -> Result<String, String> {
    let mut chat = vec![json!({ "role": "system", "content": system })];
    chat.extend(messages.iter().map(|message| json!({ "role": message.role, "content": message.content })));
    let mut body = json!({ "model": model, "messages": chat, "stream": true, "think": false });
    let url = format!("{OLLAMA_URL}/api/chat");
    let agent = ollama(None); // No timeout: loading a model can take minutes on a CPU.
    let send = |body: &Value| {
        agent
            .post(&url)
            .send_json(body)
            .map_err(|error| format!("No se pudo conectar con Ollama. ¿Está en marcha? ({error})"))
    };
    let mut response = send(&body)?;
    let mut failure = response_error(&mut response);
    // Models that cannot think reject `think`: ask again without it.
    if failure.as_deref().is_some_and(|error| error.contains("think")) {
        body.as_object_mut().expect("body is an object").remove("think");
        response = send(&body)?;
        failure = response_error(&mut response);
    }
    if let Some(error) = failure {
        return Err(format!("Ollama: {error}"));
    }

    let mut answer = String::new();
    let mut done = false;
    for line in BufReader::new(response.into_body().into_reader()).lines() {
        // Cancelled: stop reading. Dropping the response closes the connection and Ollama stops.
        if !answers.is_live(request_id) {
            break;
        }
        let line = line.map_err(|error| format!("La respuesta de Ollama se cortó: {error}"))?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(error) = event["error"].as_str() {
            return Err(format!("Ollama: {error}"));
        }
        // Only the answer: thinking models put their reasoning in `message.thinking`.
        if let Some(text) = event["message"]["content"].as_str().filter(|text| !text.is_empty()) {
            answer.push_str(text);
            let _ = on_text.send(text.to_string());
        }
        done = event["done"] == true;
    }
    if !done {
        Err("La respuesta de Ollama se cortó antes de terminar.".into())
    } else if answer.trim().is_empty() {
        Err("Ollama no devolvió respuesta.".into())
    } else {
        Ok(answer)
    }
}

/// Talks to the local Ollama directly (no proxy from the environment). HTTP errors come back
/// as responses, so their `error` text can be shown.
fn ollama(timeout: Option<Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .timeout_global(timeout)
        .build()
        .into()
}

/// The `error` text of a failed Ollama response, or `None` if it succeeded.
fn response_error(response: &mut ureq::http::Response<ureq::Body>) -> Option<String> {
    let status = response.status();
    if status.is_success() {
        return None;
    }
    let body: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    Some(body["error"].as_str().map(String::from).unwrap_or_else(|| format!("error HTTP {status}")))
}

/// Models in Ollama that can chat; empty if Ollama is not running.
#[tauri::command]
async fn ollama_models() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(|| -> Option<Vec<String>> {
        let tags: Value = ollama(Some(Duration::from_secs(3)))
            .get(format!("{OLLAMA_URL}/api/tags"))
            .call()
            .ok()?
            .body_mut()
            .read_json()
            .ok()?;
        let models = tags["models"].as_array()?;
        // Embedding-only models cannot answer. Older Ollama versions list no capabilities.
        let can_chat = |model: &&Value| {
            model["capabilities"]
                .as_array()
                .is_none_or(|capabilities| capabilities.iter().any(|capability| capability == "completion"))
        };
        Some(models.iter().filter(can_chat).filter_map(|model| model["name"].as_str().map(String::from)).collect())
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
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

/// The installed app has no console, so errors must be shown in a window.
fn show_error(message: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(message), w!("Explicador"), MB_OK | MB_ICONERROR);
    }
}

/// Toggles "Iniciar con Windows" and shows in the tray menu what Windows really has.
fn toggle_autostart<R: Runtime>(app: &AppHandle<R>, item: &CheckMenuItem<R>) {
    let autolaunch = app.autolaunch();
    let result = if autolaunch.is_enabled().unwrap_or(false) {
        autolaunch.disable()
    } else {
        autolaunch.enable()
    };
    let _ = item.set_checked(autolaunch.is_enabled().unwrap_or(false));
    if let Err(error) = result {
        show_error(&format!("No se pudo cambiar el inicio con Windows.\n\n{error}"));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyX);

    tauri::Builder::default()
        // First plugin, as its docs require: launching the app again (e.g. from the Start menu)
        // just opens the popup of the copy already running.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Already on screen: just bring it forward and keep the conversation.
            if let Some(window) = app.get_webview_window("main") {
                if window.is_visible().unwrap_or(false) {
                    let _ = window.set_focus();
                    return;
                }
            }
            let empty = Selection { text: String::new(), window_title: String::new() };
            let _ = app.emit("explain", empty);
            show_popup_near_cursor(app);
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
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
        .manage(Answers::default())
        .invoke_handler(tauri::generate_handler![
            ask_claude_code,
            ask_codex,
            ask_ollama,
            ollama_models,
            cancel_answer
        ])
        .on_window_event(|window, event| {
            // Alt+F4 hides the popup instead of quitting; quit from the tray icon.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            if let Err(error) = app.global_shortcut().register(shortcut) {
                show_error(&format!(
                    "No se pudo registrar el atajo {SHORTCUT_NAME}. Puede que otra app lo esté usando.\n\n{error}"
                ));
                return Err(error.into());
            }

            // Off by default: starting with Windows is the user's choice, from the tray menu.
            // Not from a dev build: it would register the dev executable under the same name.
            let installed = !cfg!(debug_assertions);
            let starts_with_windows = app.autolaunch().is_enabled().unwrap_or(false);
            let label = if installed { "Iniciar con Windows" } else { "Iniciar con Windows (solo en la app instalada)" };
            let autostart = CheckMenuItem::with_id(
                app, "autostart", label, installed, starts_with_windows, None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Salir", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&autostart, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip(format!("Explicador: selecciona un texto y pulsa {SHORTCUT_NAME}"))
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "autostart" => toggle_autostart(app, &autostart),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}


