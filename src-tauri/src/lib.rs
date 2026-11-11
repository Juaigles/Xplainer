use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};
#[cfg(windows)]
use std::sync::atomic::AtomicU64;
use tauri::{
    ipc::Channel,
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, PhysicalPosition, Runtime, WindowEvent,
};
use screen_context::ScreenContext;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows::{
    core::{w, HSTRING},
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::{
        System::{
            DataExchange::GetClipboardSequenceNumber,
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        },
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

#[cfg(target_os = "linux")]
mod linux;
mod screen_context;
#[cfg(target_os = "linux")]
use linux::{ollama_app_path, open_login, show_error, total_memory_gb};

const SHORTCUT_NAME: &str = "Alt+X";

/// For text that cannot be selected (in an image, a video, a locked PDF): snip it and read it.
/// Windows only: it uses Windows' snipping tool and OCR.
const SNIP_SHORTCUT_NAME: &str = "Win+Shift+Z";

/// Run as `xplainer --explain`, it explains the selection like the shortcut: on Wayland, where
/// apps cannot register global shortcuts, the user binds this command to one in their desktop.
const EXPLAIN_ARG: &str = "--explain";

#[cfg(windows)]
const STARTUP_LABEL: &str = "Iniciar con Windows";
#[cfg(target_os = "linux")]
const STARTUP_LABEL: &str = "Iniciar con la sesión";

/// Two overlapping captures would race on the clipboard and could lose the user's copied text.
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// Numbers each Win+Shift+Z. A snip still waiting gives up when a newer one starts, so pressing
/// it again after cancelling a snip (Esc) works at once.
#[cfg(windows)]
static LATEST_SNIP: AtomicU64 = AtomicU64::new(0);

/// What the popup receives: the selected text, the title of the window it came from and, for
/// Alt+X, what surrounds the selection there.
#[derive(Clone, Serialize)]
struct Selection {
    text: String,
    window_title: String,
    context: Option<ScreenContext>,
}

/// Hides the console window that Windows would open for a child process.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const OLLAMA_URL: &str = "http://127.0.0.1:11434";
/// How long a local model stays in memory after its last use. Ollama's default is 5 minutes,
/// and reloading a big model on a CPU takes about a minute.
const OLLAMA_KEEP_ALIVE: &str = "30m";

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
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    // On Linux the child leads its own process group (`background`): signal the whole group.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Prepares a child process to run unseen and stoppable: on Windows without a console window; on
/// Linux in its own process group, so `kill_tree` also reaches what it launches (node → codex).
fn background(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    command
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
    let spawned = background(&mut command)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    outcome_result(outcome, status.success(), stderr, name)
}

/// The answer, or the error to show: a reported failure wins; with no answer, the last
/// recoverable error or stderr explains why.
fn outcome_result(outcome: Outcome, exited_ok: bool, stderr: String, name: &str) -> Result<String, String> {
    let Outcome { answer, failure, last_error } = outcome;
    match (answer, failure) {
        (_, Some(message)) if !message.is_empty() => Err(format!("{name}: {message}")),
        (Some(text), None) if exited_ok && !text.trim().is_empty() => Ok(text),
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
        let command = claude_command(&model, &effort, &system);
        run_cli(answers, request_id, command, &prompt, "Claude Code", |event, outcome| {
            on_claude_event(event, outcome, &mut |text| {
                let _ = on_text.send(text.to_string());
            })
        })
    })
    .await
}

fn claude_command(model: &str, effort: &str, system: &str) -> Command {
    let mut command = Command::new("claude");
    command.args([
        "-p", "--model", model, "--effort", effort, "--system-prompt", system,
        "--output-format", "stream-json", "--include-partial-messages", "--verbose",
        // A plain question-answer call: no tools, MCP servers, skills or saved session.
        // No setting sources at all: with "project", Claude Code would walk up from the
        // working directory and load the user's own ~/.claude/CLAUDE.md as instructions.
        "--tools", "", "--strict-mcp-config", "--disable-slash-commands",
        "--setting-sources", "", "--no-session-persistence",
    ]);
    // Use the subscription login: with these set, `claude -p` would bill the API instead.
    command.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
    command
}

/// One line of `claude -p --output-format stream-json`: partial text, or the final result.
fn on_claude_event(event: &Value, outcome: &mut Outcome, on_text: &mut impl FnMut(&str)) {
    match event["type"].as_str() {
        Some("stream_event") => {
            let delta = &event["event"]["delta"];
            if delta["type"] == "text_delta" {
                if let Some(text) = delta["text"].as_str() {
                    on_text(text);
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
        let command = codex_command(&model, &effort)?;
        run_cli(answers, request_id, command, &prompt, "Codex", |event, outcome| {
            on_codex_event(event, outcome, &mut |text| {
                let _ = on_text.send(text.to_string());
            })
        })
    })
    .await
}

/// On Windows through `cmd`, so it also finds a `codex.cmd` launcher (npm, Volta); every argument
/// must then be a plain word. On Linux the npm launcher is an executable script on the PATH.
fn codex() -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/d", "/c", "codex"]);
        command
    } else {
        Command::new("codex")
    }
}

fn codex_command(model: &str, effort: &str) -> Result<Command, String> {
    let effort_setting = format!("model_reasoning_effort={}", plain_word(effort)?);
    let mut command = codex();
    command.args([
        "exec", "--json", "--ephemeral", "--skip-git-repo-check",
        // A plain question-answer call: no user config or rules, project instructions,
        // shell, app connectors, writes or web. The selected text is untrusted input.
        "--ignore-user-config", "--ignore-rules", "-s", "read-only",
        "--disable", "shell_tool", "--disable", "apps", "-c", "project_doc_max_bytes=0",
        "-c", "web_search=disabled", "-c", &effort_setting, "-m", plain_word(model)?, "-",
    ]);
    // Use the ChatGPT login: with this set, Codex would bill the API instead.
    command.env_remove("CODEX_API_KEY");
    Ok(command)
}

/// One line of `codex exec --json`.
fn on_codex_event(event: &Value, outcome: &mut Outcome, on_text: &mut impl FnMut(&str)) {
    match event["type"].as_str() {
        Some("item.completed") if event["item"]["type"] == "agent_message" => {
            let text = event["item"]["text"].as_str().unwrap_or_default().to_string();
            on_text(&text);
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
        let answer = run_ollama(answers, request_id, &model, &system, &messages, |text| {
            let _ = on_text.send(text.to_string());
        });
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
    on_text: impl FnMut(&str),
) -> Result<String, String> {
    let mut chat = vec![json!({ "role": "system", "content": system })];
    chat.extend(messages.iter().map(|message| json!({ "role": message.role, "content": message.content })));
    let mut body = json!({
        "model": model, "messages": chat, "stream": true, "think": false, "keep_alive": OLLAMA_KEEP_ALIVE,
    });
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

    // Cancelled: stop reading. Dropping the response closes the connection and Ollama stops.
    read_ollama_stream(
        BufReader::new(response.into_body().into_reader()),
        || answers.is_live(request_id),
        on_text,
    )
}

/// Reads Ollama's streamed chat (one JSON object per line) while `keep_going` holds.
fn read_ollama_stream(
    stream: impl BufRead,
    keep_going: impl Fn() -> bool,
    mut on_text: impl FnMut(&str),
) -> Result<String, String> {
    let mut answer = String::new();
    let mut done = false;
    for line in stream.lines() {
        if !keep_going() {
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
            on_text(text);
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

/// Loads a local model into memory ahead of the first question (Ollama loads a model when it
/// gets a chat with no messages). Waits until it is loaded, which can take minutes on a CPU.
#[tauri::command]
async fn preload_ollama(model: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || preload(&model))
        .await
        .map_err(|error| error.to_string())?
}

fn preload(model: &str) -> Result<(), String> {
    let body = json!({ "model": model, "messages": [], "keep_alive": OLLAMA_KEEP_ALIVE });
    let mut response = ollama(None)
        .post(format!("{OLLAMA_URL}/api/chat"))
        .send_json(&body)
        .map_err(|error| error.to_string())?;
    match response_error(&mut response) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Whether the model is already in memory, so the popup can say it is loading instead of thinking.
#[tauri::command]
async fn ollama_loaded(model: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || is_loaded(&model)).await.unwrap_or(false)
}

fn is_loaded(model: &str) -> bool {
    let running: Option<Value> = ollama(Some(Duration::from_secs(2)))
        .get(format!("{OLLAMA_URL}/api/ps"))
        .call()
        .ok()
        .and_then(|mut response| response.body_mut().read_json().ok());
    running.is_some_and(|running| listed_as_loaded(&running, model))
}

/// Whether `/api/ps` lists the model among those in memory.
fn listed_as_loaded(running: &Value, model: &str) -> bool {
    running["models"].as_array().is_some_and(|models| {
        models.iter().any(|loaded| loaded["name"] == model || loaded["model"] == model)
    })
}

/// Models in Ollama that can chat; `None` if Ollama is not running.
fn fetch_ollama_models() -> Option<Vec<String>> {
    let tags: Value = ollama(Some(Duration::from_secs(3)))
        .get(format!("{OLLAMA_URL}/api/tags"))
        .call()
        .ok()?
        .body_mut()
        .read_json()
        .ok()?;
    chat_models(&tags)
}

/// The models in an `/api/tags` answer that can chat.
fn chat_models(tags: &Value) -> Option<Vec<String>> {
    let models = tags["models"].as_array()?;
    // Embedding-only models cannot answer. Older Ollama versions list no capabilities.
    let can_chat = |model: &&Value| {
        model["capabilities"]
            .as_array()
            .is_none_or(|capabilities| capabilities.iter().any(|capability| capability == "completion"))
    };
    Some(models.iter().filter(can_chat).filter_map(|model| model["name"].as_str().map(String::from)).collect())
}

#[cfg(windows)]
const OLLAMA_DOWNLOAD_URL: &str = "https://ollama.com/download/windows";
#[cfg(target_os = "linux")]
const OLLAMA_DOWNLOAD_URL: &str = "https://ollama.com/download/linux";

/// What the first-run assistant needs to know about this computer.
#[derive(Serialize)]
struct Setup {
    ollama_installed: bool,
    ollama_running: bool,
    ollama_models: Vec<String>,
    /// Subscription CLIs found on the PATH: "claude", "codex".
    clis: Vec<String>,
    /// A free local model that fits this computer, and its download size.
    recommended_model: String,
    recommended_size: String,
}

/// Ollama's tray app, which starts its server: in the installer's default folder, or next to
/// the `ollama.exe` on the PATH when it was installed elsewhere (installer `/DIR=...`).
#[cfg(windows)]
fn ollama_app_path() -> Option<PathBuf> {
    let default = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Programs")
        .join("Ollama")
        .join("ollama app.exe");
    if default.exists() {
        return Some(default);
    }
    let output = Command::new("where")
        .arg("ollama.exe")
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let ollama = PathBuf::from(String::from_utf8_lossy(&output.stdout).lines().next()?.trim());
    let app = ollama.parent()?.join("ollama app.exe");
    app.exists().then_some(app)
}

#[cfg(target_os = "linux")]
fn on_path(program: &str) -> bool {
    linux::find_program(program).is_some()
}

#[cfg(windows)]
fn on_path(program: &str) -> bool {
    background(&mut Command::new("where"))
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Opens a web address in the default browser, with no shell to interpret it.
fn open_url(url: &str) -> Result<(), String> {
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    Command::new(opener).arg(url).spawn().map(|_| ()).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn total_memory_gb() -> u64 {
    let mut memory = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    match unsafe { GlobalMemoryStatusEx(&mut memory) } {
        Ok(()) => memory.ullTotalPhys / (1024 * 1024 * 1024),
        Err(_) => 0,
    }
}

/// Small instruct models: they answer without "thinking" first, so they are quick even on a
/// laptop CPU, and they handle Spanish well.
fn recommended_model() -> (&'static str, &'static str) {
    recommended_for(total_memory_gb())
}

fn recommended_for(memory_gb: u64) -> (&'static str, &'static str) {
    if memory_gb >= 10 {
        ("qwen3:4b-instruct", "2,5 GB")
    } else {
        ("qwen3:1.7b", "1,4 GB")
    }
}

#[tauri::command]
async fn check_setup() -> Result<Setup, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let models = fetch_ollama_models();
        let (model, size) = recommended_model();
        Setup {
            // Installed means something `start_ollama` can open.
            ollama_installed: models.is_some() || ollama_app_path().is_some(),
            ollama_running: models.is_some(),
            ollama_models: models.unwrap_or_default(),
            // Exactly what `ask_*` can launch. On Windows `claude` must be the native
            // `claude.exe` (an npm `claude.cmd` would be found by `where claude` but cannot be
            // started directly); `codex` goes through `cmd`, which also finds a `codex.cmd`.
            clis: [("claude", if cfg!(windows) { "claude.exe" } else { "claude" }), ("codex", "codex")]
                .into_iter()
                .filter(|(_, program)| on_path(program))
                .map(|(cli, _)| cli.to_string())
                .collect(),
            recommended_model: model.into(),
            recommended_size: size.into(),
        }
    })
    .await
    .map_err(|error| error.to_string())
}

/// Opens Ollama's official download page; the user installs it from there.
#[tauri::command]
fn open_ollama_download() -> Result<(), String> {
    open_url(OLLAMA_DOWNLOAD_URL)
}

/// Starts Ollama when it is installed but closed (it normally starts with Windows).
#[cfg(windows)]
#[tauri::command]
fn start_ollama() -> Result<(), String> {
    let app = ollama_app_path().ok_or("No encuentro la app de Ollama. Ábrela desde el menú Inicio.")?;
    Command::new(app).spawn().map(|_| ()).map_err(|error| format!("No se pudo abrir Ollama: {error}"))
}

#[cfg(target_os = "linux")]
#[tauri::command]
fn start_ollama() -> Result<(), String> {
    linux::start_ollama()
}

/// Subscriptions signed in, so they answer with the user's plan: "claude", "codex".
#[tauri::command]
async fn check_accounts() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut claude = Command::new("claude");
        // As when asking: without the API keys, so the status is the subscription's.
        claude.args(["auth", "status", "--json"]).env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN");
        let mut codex = codex();
        codex.args(["login", "status"]).env_remove("CODEX_API_KEY");
        // At the same time: each may wait on the network to refresh its login.
        let timeout = Duration::from_secs(10);
        let claude = thread::spawn(move || status_output(claude, timeout));
        let codex = status_output(codex, timeout);
        let claude = claude.join().ok().flatten();
        let mut signed_in = Vec::new();
        if claude.is_some_and(|output| claude_uses_subscription(&output.stdout)) {
            signed_in.push("claude".to_string());
        }
        if codex.is_some_and(|output| output.status.success() && codex_uses_subscription(&output.stdout, &output.stderr)) {
            signed_in.push("codex".to_string());
        }
        signed_in
    })
    .await
    .map_err(|error| error.to_string())
}

/// Runs a login status command, or gives up after `timeout` (a stalled network, say): otherwise
/// the first-run choice would stay "checking" forever. Its output is a few lines, well within
/// the pipe buffer, so waiting before reading cannot block the command.
fn status_output(mut command: Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = background(&mut command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() > deadline {
            kill_tree(&mut child);
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
    child.wait_with_output().ok()
}

/// `claude auth status --json`: signed in with a Claude plan ("claude.ai"), not with an
/// Anthropic Console account, which bills the API.
fn claude_uses_subscription(stdout: &[u8]) -> bool {
    serde_json::from_slice::<Value>(stdout)
        .is_ok_and(|status| status["loggedIn"] == true && status["authMethod"] == "claude.ai")
}

/// `codex login status` prints "Logged in using ChatGPT" (on stderr) or "... an API key", which
/// bills the API.
fn codex_uses_subscription(stdout: &[u8], stderr: &[u8]) -> bool {
    [stdout, stderr].iter().any(|output| String::from_utf8_lossy(output).contains("using ChatGPT"))
}

/// Opens a console window with the CLI's own login, which continues in the browser. Xplainer
/// never sees the account: the CLI keeps it. `|| pause` keeps the window open only on an error.
#[tauri::command]
fn sign_in(cli: String) -> Result<(), String> {
    let login: &[&str] = match cli.as_str() {
        "claude" => &["claude", "auth", "login", "--claudeai"],
        "codex" => &["codex", "login"],
        _ => return Err(format!("No sé iniciar sesión en «{cli}».")),
    };
    open_login(login)
}

#[cfg(windows)]
fn open_login(login: &[&str]) -> Result<(), String> {
    Command::new("cmd")
        .args(["/d", "/c"])
        .args(login)
        .args(["||", "pause"])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("No se pudo abrir el inicio de sesión: {error}"))
}

/// Opens the official install guide of a subscription CLI.
#[tauri::command]
fn open_install_page(cli: String) -> Result<(), String> {
    let url = match cli.as_str() {
        "claude" => "https://code.claude.com/docs/en/setup",
        "codex" => "https://developers.openai.com/codex/cli",
        _ => return Err(format!("No conozco «{cli}».")),
    };
    open_url(url)
}

/// Downloads a model into Ollama, reporting progress (bytes done and total) as it goes.
#[tauri::command]
async fn pull_ollama_model(model: String, on_progress: Channel<Value>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut response = ollama(None)
            .post(format!("{OLLAMA_URL}/api/pull"))
            .send_json(json!({ "model": model, "stream": true }))
            .map_err(|error| format!("No se pudo conectar con Ollama. ¿Está en marcha? ({error})"))?;
        if let Some(error) = response_error(&mut response) {
            return Err(format!("Ollama: {error}"));
        }
        for line in BufReader::new(response.into_body().into_reader()).lines() {
            let line = line.map_err(|error| format!("La descarga se cortó: {error}"))?;
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(error) = event["error"].as_str() {
                return Err(format!("Ollama: {error}"));
            }
            if event["status"] == "success" {
                return Ok(());
            }
            let _ = on_progress.send(json!({ "completed": event["completed"], "total": event["total"] }));
        }
        Err("La descarga terminó sin confirmar el modelo.".into())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Saves an explanation as a Markdown note in the user's notes folder (an Obsidian vault, say).
/// With `existing`, a note saved before from the same conversation, it rewrites that note;
/// otherwise it creates a new one and never overwrites another.
#[tauri::command]
fn save_note(folder: String, title: String, content: String, existing: Option<String>) -> Result<String, String> {
    let folder = PathBuf::from(folder.trim());
    if !folder.is_dir() {
        return Err(format!("No encuentro la carpeta de notas «{}». Revísala en Ajustes.", folder.display()));
    }
    let failed = |error: std::io::Error| format!("No se pudo guardar la nota: {error}");
    let previous = existing
        .map(PathBuf::from)
        .filter(|path| path.parent() == Some(folder.as_path()) && path.is_file());
    if let Some(path) = previous {
        fs::write(&path, content).map_err(failed)?;
        return Ok(path.display().to_string());
    }
    let name = note_name(&title);
    let mut copy = 1;
    loop {
        let file = if copy == 1 { format!("{name}.md") } else { format!("{name} {copy}.md") };
        let path = folder.join(file);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut note) => {
                note.write_all(content.as_bytes()).map_err(failed)?;
                return Ok(path.display().to_string());
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => copy += 1,
            Err(error) => return Err(failed(error)),
        }
    }
}

/// The history of explanations, kept only on this computer, in the app's data folder.
fn history_file(app: &AppHandle) -> Result<PathBuf, String> {
    let folder = app.path().app_data_dir().map_err(|error| error.to_string())?;
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    Ok(folder.join("history.json"))
}

/// The history as JSON text; an empty list the first time.
#[tauri::command]
fn read_history(app: AppHandle) -> Result<String, String> {
    read_history_at(&history_file(&app)?)
}

fn read_history_at(file: &Path) -> Result<String, String> {
    match fs::read_to_string(file) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok("[]".into()),
        Err(error) => Err(format!("No se pudo leer el historial: {error}")),
    }
}

/// Replaces the whole history. Through a temporary file, so a crash never leaves it half written.
#[tauri::command]
fn write_history(app: AppHandle, history: String) -> Result<(), String> {
    write_history_at(&history_file(&app)?, &history)
}

fn write_history_at(file: &Path, history: &str) -> Result<(), String> {
    let temporary = file.with_extension("json.tmp");
    fs::write(&temporary, history)
        .and_then(|()| fs::rename(&temporary, file))
        .map_err(|error| format!("No se pudo guardar el historial: {error}"))
}

/// A file name that Windows accepts and Obsidian can link to, from the note's title.
fn note_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*#^[]"#.contains(c) { ' ' } else { c })
        .collect();
    let words = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = words.chars().take(60).collect::<String>().trim_end_matches(['.', ' ']).to_string();
    // Device names such as CON or NUL cannot be file names on Windows, whatever the extension.
    let upper = name.to_uppercase();
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.ends_with(|c: char| c.is_ascii_digit()));
    if name.is_empty() {
        "Nota".into()
    } else if device {
        format!("{name} (nota)")
    } else {
        name
    }
}

#[cfg(windows)]
fn foreground_window_title() -> String {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

/// The user is still holding the shortcut keys; a Ctrl+C sent now would arrive as Win+Alt+Ctrl+C.
/// Returns false if they are still held after the deadline.
#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
fn send_ctrl_c() {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VK_C, false),
        key_input(VK_C, true),
        key_input(VK_CONTROL, true),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// The shortcut swallows the X, so if Alt is released first the source app sees Alt pressed
/// alone and opens its menu, which would eat our Ctrl+C. Tapping an unassigned key while Alt
/// is still held prevents that (the "menu mask" trick AutoHotkey uses).
#[cfg(windows)]
fn mask_alt_menu() {
    let unassigned = VIRTUAL_KEY(0xE8);
    let inputs = [key_input(unassigned, false), key_input(unassigned, true)];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

/// Windows has no universal "read the selection" API, so we copy it with a simulated
/// Ctrl+C and then put the previous clipboard text back.
#[cfg(windows)]
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

#[cfg(windows)]
fn explain_selection(app: &AppHandle) {
    mask_alt_menu();
    let window_title = foreground_window_title();
    let in_popup = popup_is_foreground(app);
    // Read while the selection is copied, before the popup takes the focus.
    let pending = if in_popup { None } else { screen_context::start(unsafe { GetForegroundWindow() }) };
    // Keys still held: don't inject anything; the popup opens empty so the user can type the term.
    let text = if wait_for_modifiers_release() {
        capture_selection()
    } else {
        String::new()
    };
    if in_popup {
        // A word selected in an answer: explain it one level deeper, without moving the popup.
        let _ = app.emit("explain-deeper", Selection { text, window_title, context: None });
        return;
    }
    let context = pending.and_then(|pending| pending.finish());
    let _ = app.emit("explain", Selection { text, window_title, context });
    show_popup_near_cursor(app);
}

/// Opens Windows' snipping tool and waits for the snip it copies to the clipboard, which keeps it
/// as Win+Shift+S would. `None` if the user cancels (nothing arrives in 30 seconds or a newer
/// snip starts) or copies something else meanwhile.
#[cfg(windows)]
fn snip_screen(snip: u64) -> Option<arboard::ImageData<'static>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let sequence_before = unsafe { GetClipboardSequenceNumber() };
    Command::new("explorer").arg("ms-screenclip:").spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while unsafe { GetClipboardSequenceNumber() } == sequence_before {
        if Instant::now() > deadline || LATEST_SNIP.load(Ordering::SeqCst) != snip {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_millis(100)); // Let the snipping tool finish writing.
    clipboard.get_image().ok() // Text instead: the user copied something else.
}

/// Reads the text in an image with the OCR built into Windows, keeping its lines.
#[cfg(windows)]
fn recognize_text(image: &arboard::ImageData) -> windows::core::Result<String> {
    let mut pixels = image.bytes.to_vec();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2); // arboard gives RGBA; the bitmap wants BGRA.
    }
    let writer = DataWriter::new()?;
    writer.WriteBytes(&pixels)?;
    let (width, height) = (image.width as i32, image.height as i32);
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&writer.DetachBuffer()?, BitmapPixelFormat::Bgra8, width, height)?;
    // Uses the OCR languages installed with the user's Windows display languages.
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
    let result = engine.RecognizeAsync(&bitmap)?.join()?;
    let mut lines = Vec::new(); // (top, bottom, left, text)
    for line in result.Lines()? {
        let boxes = line.Words()?.into_iter().map(|word| word.BoundingRect()).collect::<windows::core::Result<Vec<_>>>()?;
        let top = boxes.iter().map(|b| b.Y).fold(f32::MAX, f32::min);
        let bottom = boxes.iter().map(|b| b.Y + b.Height).fold(0.0, f32::max);
        let left = boxes.iter().map(|b| b.X).fold(f32::MAX, f32::min);
        lines.push((top, bottom, left, line.Text()?.to_string()));
    }
    Ok(rows(lines))
}

/// The OCR splits a line where it sees a wide gap (common in code) and may list the halves out of
/// order: join the pieces that share a row, left to right, and the rows top to bottom.
#[cfg(windows)]
fn rows(mut lines: Vec<(f32, f32, f32, String)>) -> String {
    lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut rows: Vec<(f32, Vec<(f32, String)>)> = Vec::new(); // (middle, pieces)
    for (top, bottom, left, text) in lines {
        match rows.last_mut() {
            Some((middle, pieces)) if top < *middle => pieces.push((left, text)),
            _ => rows.push(((top + bottom) / 2.0, vec![(left, text)])),
        }
    }
    let joined = rows.into_iter().map(|(_, mut pieces)| {
        pieces.sort_by(|a, b| a.0.total_cmp(&b.0));
        pieces.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join(" ")
    });
    joined.collect::<Vec<_>>().join("\n")
}

#[cfg(windows)]
fn explain_snip(app: &AppHandle, snip: u64) {
    let window_title = foreground_window_title();
    let Some(image) = snip_screen(snip) else {
        return;
    };
    let failure = match recognize_text(&image) {
        Ok(text) if !text.trim().is_empty() => {
            let _ = app.emit("explain", Selection { text, window_title, context: None });
            None
        }
        Ok(_) => Some("No he encontrado texto en el recorte.".to_string()),
        Err(error) => Some(format!("No se pudo leer el texto del recorte: {}", error.message())),
    };
    if let Some(message) = failure {
        let _ = app.emit("snip-failed", message);
    }
    show_popup_near_cursor(app);
}

/// "Usar el contexto de la ventana", from the settings.
#[tauri::command]
fn set_screen_context(enabled: bool) {
    screen_context::ENABLED.store(enabled, Ordering::SeqCst);
}

#[cfg(windows)]
fn popup_is_foreground(app: &AppHandle) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    app.get_webview_window("main")
        .and_then(|window| window.hwnd().ok())
        .is_some_and(|popup| std::ptr::eq(popup.0, foreground.0))
}

#[cfg(target_os = "linux")]
fn popup_is_foreground(app: &AppHandle) -> bool {
    app.get_webview_window("main").is_some_and(|window| window.is_focused().unwrap_or(false))
}

/// On Linux nothing is copied: on Wayland no app may fake a Ctrl+C, and X11 keeps the selection
/// readable anyway. The accessibility bus says what is selected now, and the PRIMARY selection
/// completes it (`linux::current_selection`).
#[cfg(target_os = "linux")]
fn explain_selection(app: &AppHandle) {
    if popup_is_foreground(app) {
        // A word selected in an answer: the popup reads its own selection and goes one level deeper.
        let _ = app.emit("explain-deeper", Selection { text: String::new(), window_title: String::new(), context: None });
        return;
    }
    let window = screen_context::start().and_then(|pending| pending.finish());
    let (window_title, seen, context) = match window {
        Some(window) => (window.title, window.selected, window.context),
        None => (String::new(), linux::Selected::Unknown, None),
    };
    let text = linux::current_selection(seen, linux::primary_selection());
    let _ = app.emit("explain", Selection { text, window_title, context });
    show_popup_near_cursor(app);
}

/// The installed app has no console, so errors must be shown in a window.
#[cfg(windows)]
fn show_error(message: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(message), w!("Xplainer"), MB_OK | MB_ICONERROR);
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
        show_error(&format!("No se pudo cambiar «{STARTUP_LABEL}».\n\n{error}"));
    }
}

/// The key that explains a selection, or `None` on Wayland, which lets no app register a global
/// shortcut: there the user binds `xplainer --explain` to one in their desktop's settings.
fn explain_key() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    if linux::is_wayland() {
        return None;
    }
    Some(SHORTCUT_NAME)
}

/// The shortcuts this computer has, for the popup's hints.
#[derive(Serialize)]
struct Shortcuts {
    explain: Option<&'static str>,
    snip: Option<&'static str>,
}

#[tauri::command]
fn shortcuts() -> Shortcuts {
    Shortcuts { explain: explain_key(), snip: cfg!(windows).then_some(SNIP_SHORTCUT_NAME) }
}

/// Set when this copy was started by `xplainer --explain` (the desktop shortcut, with the app
/// closed): the selection is explained once the popup listens, or the event would be lost.
static EXPLAIN_ON_START: AtomicBool = AtomicBool::new(false);

/// The popup is listening for `explain`.
#[tauri::command]
fn listening(app: AppHandle) {
    if EXPLAIN_ON_START.swap(false, Ordering::SeqCst) {
        explain_in_background(&app);
    }
}

/// Explains the selection off the event loop (capturing it waits and sleeps), one at a time.
fn explain_in_background(app: &AppHandle) {
    if CAPTURING.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    thread::spawn(move || {
        explain_selection(&app);
        CAPTURING.store(false, Ordering::SeqCst);
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Before any other thread starts: it changes this process's environment. Not when this copy
    // only hands `--explain` to the one running: the shell would make the shortcut wait.
    #[cfg(target_os = "linux")]
    if !(std::env::args().any(|arg| arg == EXPLAIN_ARG) && linux::already_running()) {
        linux::use_login_path();
    }
    EXPLAIN_ON_START.store(std::env::args().any(|arg| arg == EXPLAIN_ARG), Ordering::SeqCst);
    let shortcut = Shortcut::new(Some(Modifiers::ALT), Code::KeyX);
    #[cfg(windows)]
    let snip_shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyZ);

    tauri::Builder::default()
        // First plugin, as its docs require: launching the app again (e.g. from the Start menu)
        // just opens the popup of the copy already running.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // `xplainer --explain`, from a desktop shortcut: explain the selection.
            if args.iter().any(|arg| arg == EXPLAIN_ARG) {
                explain_in_background(app);
                return;
            }
            // Already on screen: just bring it forward and keep the conversation.
            if let Some(window) = app.get_webview_window("main") {
                if window.is_visible().unwrap_or(false) {
                    let _ = window.set_focus();
                    return;
                }
            }
            let empty = Selection { text: String::new(), window_title: String::new(), context: None };
            let _ = app.emit("explain", empty);
            show_popup_near_cursor(app);
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, pressed, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if pressed == &shortcut {
                        explain_in_background(app);
                    }
                    #[cfg(windows)]
                    if pressed == &snip_shortcut {
                        let snip = LATEST_SNIP.fetch_add(1, Ordering::SeqCst) + 1;
                        let app = app.clone();
                        // Off the event loop: it waits for the user to snip.
                        thread::spawn(move || explain_snip(&app, snip));
                    }
                })
                .build(),
        )
        .manage(Answers::default())
        .invoke_handler(tauri::generate_handler![
            ask_claude_code,
            ask_codex,
            ask_ollama,
            check_setup,
            check_accounts,
            sign_in,
            open_install_page,
            open_ollama_download,
            start_ollama,
            pull_ollama_model,
            preload_ollama,
            ollama_loaded,
            save_note,
            read_history,
            write_history,
            set_screen_context,
            shortcuts,
            listening,
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
            if explain_key().is_some() {
                if let Err(error) = app.global_shortcut().register(shortcut) {
                    show_error(&format!(
                        "No se pudo registrar el atajo {SHORTCUT_NAME}. Puede que otra app lo esté usando.\n\n{error}"
                    ));
                    return Err(error.into());
                }
            }
            // Not essential: without it, Alt+X still works.
            #[cfg(windows)]
            if let Err(error) = app.global_shortcut().register(snip_shortcut) {
                show_error(&format!(
                    "No se pudo registrar el atajo {SNIP_SHORTCUT_NAME} para explicar el texto de un recorte. Puede que otra app lo esté usando; {SHORTCUT_NAME} sigue funcionando.\n\n{error}"
                ));
            }

            // Off by default: starting with Windows is the user's choice, from the tray menu.
            // Not from a dev build: it would register the dev executable under the same name.
            let installed = !cfg!(debug_assertions);
            let starts_with_windows = app.autolaunch().is_enabled().unwrap_or(false);
            let label =
                if installed { STARTUP_LABEL.to_string() } else { format!("{STARTUP_LABEL} (solo en la app instalada)") };
            let autostart = CheckMenuItem::with_id(
                app, "autostart", label, installed, starts_with_windows, None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Salir", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&autostart, &quit])?;
            let how = match explain_key() {
                Some(key) => format!("pulsa {key}"),
                None => format!("pulsa el atajo que ejecuta «xplainer {EXPLAIN_ARG}»"),
            };
            let snip = if cfg!(windows) { format!(", o recórtalo con {SNIP_SHORTCUT_NAME}") } else { String::new() };
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip(format!("Xplainer: selecciona un texto y {how}{snip}"))
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

#[cfg(test)]
mod tests;
