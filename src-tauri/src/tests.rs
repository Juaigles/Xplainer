//! Tests of the native side that need no window, no keys and no model: `cargo test`.
//! The fixtures in `tests/fixtures` are real outputs of `claude -p`, `codex exec` and Ollama,
//! recorded once with ids removed. Calls to real models are `#[ignore]`d: run them with
//! `cargo test -- --ignored --nocapture --test-threads 1`.

use super::*;
use std::io::Cursor;

const CLAUDE_STREAM: &str = include_str!("../tests/fixtures/claude-stream.jsonl");
const CLAUDE_ERROR: &str = include_str!("../tests/fixtures/claude-error.jsonl");
const CODEX_STREAM: &str = include_str!("../tests/fixtures/codex-stream.jsonl");
const CODEX_ERROR: &str = include_str!("../tests/fixtures/codex-error.jsonl");
const OLLAMA_CHAT: &str = include_str!("../tests/fixtures/ollama-chat.jsonl");
const OLLAMA_TAGS: &str = include_str!("../tests/fixtures/ollama-tags.json");
const OLLAMA_PS: &str = include_str!("../tests/fixtures/ollama-ps.json");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

/// A fresh folder for one test, removed when it ends.
struct TempFolder(PathBuf);

impl TempFolder {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("xplanator-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempFolder(path)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Feeds a recorded CLI output to an event parser, as `run_cli` does.
fn parse(jsonl: &str, mut on_event: impl FnMut(&Value, &mut Outcome)) -> Outcome {
    let mut outcome = Outcome::default();
    for line in jsonl.lines() {
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            on_event(&event, &mut outcome);
        }
    }
    outcome
}

// --- Command-line safety ----------------------------------------------------------------------

#[test]
fn plain_word_accepts_model_names_and_rejects_shell_characters() {
    for value in ["gpt-5.6-luna", "low", "qwen3:4b-instruct", "model_reasoning_effort"] {
        assert_eq!(plain_word(value), Ok(value));
    }
    for value in ["", "low & calc", "a|b", "x y", "\"quoted\"", "a^b", "%PATH%"] {
        assert!(plain_word(value).is_err(), "{value:?} should be rejected");
    }
}

fn args(command: &Command) -> Vec<String> {
    command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect()
}

fn removes_env(command: &Command, key: &str) -> bool {
    command.get_envs().any(|(name, value)| name == key && value.is_none())
}

#[test]
fn the_clis_run_isolated_and_bill_the_subscription() {
    let claude = claude_command("claude-haiku-5-5", "low", "Explica.");
    let claude_args = args(&claude);
    assert_eq!(claude.get_program(), "claude");
    for flag in ["--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence"] {
        assert!(claude_args.contains(&flag.to_string()), "{flag} missing: {claude_args:?}");
    }
    for flag in ["--tools", "--setting-sources"] {
        let at = claude_args.iter().position(|arg| arg == flag).expect(flag);
        assert_eq!(claude_args[at + 1], "", "{flag} must be empty");
    }
    // `--bare` skips the OAuth login and breaks the subscription.
    assert!(!claude_args.contains(&"--bare".to_string()));
    assert!(removes_env(&claude, "ANTHROPIC_API_KEY") && removes_env(&claude, "ANTHROPIC_AUTH_TOKEN"));

    let codex = codex_command("gpt-5.6-luna", "low").unwrap();
    let codex_args = args(&codex).join(" ");
    for flag in [
        "--ignore-user-config", "--ignore-rules", "-s read-only", "--disable shell_tool", "--disable apps",
        "-c project_doc_max_bytes=0", "-c web_search=disabled", "-m gpt-5.6-luna",
    ] {
        assert!(codex_args.contains(flag), "{flag} missing: {codex_args}");
    }
    assert!(removes_env(&codex, "CODEX_API_KEY"));
    assert!(codex_command("gpt & calc", "low").is_err(), "a model name cannot inject a command");
}

// --- Claude Code events -----------------------------------------------------------------------

#[test]
fn claude_stream_gives_partial_text_and_the_final_answer() {
    let mut streamed = String::new();
    let outcome = parse(CLAUDE_STREAM, |event, outcome| {
        on_claude_event(event, outcome, &mut |text| streamed.push_str(text))
    });
    let answer = outcome.answer.clone().expect("the result event carries the answer");
    assert!(answer.starts_with("TCP (Transmission Control Protocol)"));
    assert!(answer.contains("\n>> "), "the suggested question line arrives");
    assert_eq!(streamed, answer, "the partial text adds up to the final answer");
    assert_eq!(outcome_result(outcome, true, String::new(), "Claude Code"), Ok(answer));
}

#[test]
fn claude_error_result_becomes_a_readable_error() {
    let outcome = parse(CLAUDE_ERROR, |event, outcome| on_claude_event(event, outcome, &mut |_| {}));
    let error = outcome_result(outcome, false, String::new(), "Claude Code").unwrap_err();
    assert!(error.starts_with("Claude Code: "), "{error}");
    assert!(error.contains("claude-no-existe-1"), "{error}");
}

#[test]
fn claude_error_with_only_an_errors_list_is_reported() {
    let event = json!({ "type": "result", "is_error": true, "result": "", "errors": ["uno", "dos"] });
    let mut outcome = Outcome::default();
    on_claude_event(&event, &mut outcome, &mut |_| {});
    assert_eq!(outcome.failure.as_deref(), Some("uno; dos"));
}

// --- Codex events -----------------------------------------------------------------------------

#[test]
fn codex_stream_gives_the_agent_message() {
    let mut streamed = Vec::new();
    let outcome = parse(CODEX_STREAM, |event, outcome| {
        on_codex_event(event, outcome, &mut |text| streamed.push(text.to_string()))
    });
    assert_eq!(streamed, ["TCP es un protocolo."]);
    assert_eq!(outcome_result(outcome, true, String::new(), "Codex").as_deref(), Ok("TCP es un protocolo."));
}

#[test]
fn codex_failed_turn_is_an_error_and_warnings_are_not_answers() {
    let outcome = parse(CODEX_ERROR, |event, outcome| on_codex_event(event, outcome, &mut |_| {}));
    assert!(outcome.answer.is_none(), "an item of type error is not an answer");
    let error = outcome_result(outcome, false, String::new(), "Codex").unwrap_err();
    assert!(error.starts_with("Codex: ") && error.contains("model_not_found"), "{error}");
}

#[test]
fn codex_retry_errors_are_shown_only_when_no_answer_arrives() {
    let reconnect = json!({ "type": "error", "message": "Reconnecting... 1/5" });
    let answer = json!({ "type": "item.completed", "item": { "type": "agent_message", "text": "Hola" } });
    let mut outcome = Outcome::default();
    on_codex_event(&reconnect, &mut outcome, &mut |_| {});
    on_codex_event(&answer, &mut outcome, &mut |_| {});
    assert_eq!(outcome_result(outcome, true, String::new(), "Codex").as_deref(), Ok("Hola"));

    let mut outcome = Outcome::default();
    on_codex_event(&reconnect, &mut outcome, &mut |_| {});
    assert_eq!(
        outcome_result(outcome, false, "stderr".into(), "Codex"),
        Err("Codex no respondió. Reconnecting... 1/5".into())
    );
}

#[test]
fn an_answer_from_a_process_that_failed_is_not_trusted() {
    let outcome = Outcome { answer: Some("medio".into()), ..Outcome::default() };
    assert_eq!(outcome_result(outcome, false, "boom".into(), "Codex"), Err("Codex no respondió. boom".into()));
}

// --- A whole CLI run and the requests registry ------------------------------------------------

#[test]
fn run_cli_reads_a_real_process_and_returns_its_answer() {
    let answers = Answers::default();
    // `type` prints the recorded output: it plays the CLI, through a real child process.
    let mut command = Command::new("cmd");
    command.args(["/d", "/c", "type"]).arg(fixture("codex-stream.jsonl"));
    let answer = run_cli(&answers, 1, command, "prompt", "Codex", |event, outcome| {
        on_codex_event(event, outcome, &mut |_| {})
    });
    assert_eq!(answer.as_deref(), Ok("TCP es un protocolo."));
    assert!(!answers.is_live(1), "a finished request is no longer tracked");
}

#[test]
fn run_cli_explains_a_program_that_cannot_start() {
    let answers = Answers::default();
    let error = run_cli(&answers, 1, Command::new("no-such-cli-xplanator"), "", "Claude Code", |_, _| {});
    assert!(error.unwrap_err().starts_with("No se pudo ejecutar Claude Code"));
    assert!(answers.begin(2).is_ok(), "a failed start leaves the registry usable");
}

#[test]
fn a_cancel_that_arrives_first_still_counts() {
    let answers = Answers::default();
    answers.cancel(5);
    assert!(answers.begin(5).is_err());
    assert!(answers.begin(3).is_err(), "older requests are cancelled too");
    assert!(answers.begin(6).is_ok());
    assert!(answers.is_live(6));
}

#[test]
fn a_newer_request_replaces_the_older_one() {
    let answers = Answers::default();
    answers.begin(1).unwrap();
    answers.begin(2).unwrap();
    assert!(answers.finish(1).is_err(), "the replaced request reports cancelled");
    assert!(answers.begin(1).is_err(), "an older request cannot start after a newer one");
    assert!(answers.finish(2).is_ok());
}

#[test]
fn cancelling_kills_the_running_process() {
    let answers = Answers::default();
    answers.begin(1).unwrap();
    let child = Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    let pid = child.id();
    answers.attach(1, child).unwrap();
    answers.cancel(1);
    let still_there = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        .unwrap();
    assert!(!still_there, "the process must be gone after cancel");
    assert!(answers.finish(1).is_err());
}

// --- Ollama -----------------------------------------------------------------------------------

#[test]
fn ollama_stream_is_read_until_done() {
    let mut streamed = String::new();
    let answer = read_ollama_stream(Cursor::new(OLLAMA_CHAT), || true, |text| streamed.push_str(text)).unwrap();
    assert!(answer.contains("TCP"), "{answer}");
    assert_eq!(streamed, answer);
}

#[test]
fn ollama_stream_without_done_is_reported_as_cut() {
    let cut: String = OLLAMA_CHAT.lines().filter(|line| !line.contains("\"done\":true")).collect::<Vec<_>>().join("\n");
    let error = read_ollama_stream(Cursor::new(cut), || true, |_| {}).unwrap_err();
    assert!(error.contains("se cortó"), "{error}");
}

#[test]
fn ollama_stream_stops_when_cancelled_and_reports_errors() {
    let error = read_ollama_stream(Cursor::new(OLLAMA_CHAT), || false, |_| {}).unwrap_err();
    assert!(error.contains("se cortó"), "a cancelled read never claims a full answer");

    let failed = "{\"error\":\"model 'x' not found\"}\n";
    assert_eq!(read_ollama_stream(Cursor::new(failed), || true, |_| {}), Err("Ollama: model 'x' not found".into()));

    let empty = "{\"message\":{\"content\":\"\"},\"done\":true}\n";
    assert_eq!(read_ollama_stream(Cursor::new(empty), || true, |_| {}), Err("Ollama no devolvió respuesta.".into()));
}

#[test]
fn only_models_that_can_chat_are_listed() {
    let recorded: Value = serde_json::from_str(OLLAMA_TAGS).unwrap();
    assert_eq!(chat_models(&recorded).unwrap(), ["qwen3:30b-a3b-instruct-2507-q4_K_M"]);

    let tags = json!({ "models": [
        { "name": "nomic-embed-text", "capabilities": ["embedding"] },
        { "name": "llama3.2", "capabilities": ["completion", "tools"] },
        { "name": "old-ollama-model" },
    ] });
    assert_eq!(chat_models(&tags).unwrap(), ["llama3.2", "old-ollama-model"]);
    assert_eq!(chat_models(&json!({})), None);
}

#[test]
fn a_loaded_model_is_recognised_in_api_ps() {
    let running: Value = serde_json::from_str(OLLAMA_PS).unwrap();
    assert!(listed_as_loaded(&running, "qwen3:30b-a3b-instruct-2507-q4_K_M"));
    assert!(!listed_as_loaded(&running, "qwen3:4b-instruct"));
    assert!(!listed_as_loaded(&json!({ "models": [] }), "qwen3:4b-instruct"));
}

#[test]
fn the_recommended_model_fits_the_memory() {
    assert_eq!(recommended_for(16), ("qwen3:4b-instruct", "2,5 GB"));
    assert_eq!(recommended_for(10), ("qwen3:4b-instruct", "2,5 GB"));
    assert_eq!(recommended_for(8), ("qwen3:1.7b", "1,4 GB"));
}

// --- Notes ------------------------------------------------------------------------------------

#[test]
fn note_names_are_valid_windows_file_names() {
    assert_eq!(note_name("TCP"), "TCP");
    assert_eq!(note_name("a/b:c *d* [[x]] #y ^z"), "a b c d x y z");
    assert_eq!(note_name("con"), "con (nota)");
    assert_eq!(note_name("COM3"), "COM3 (nota)");
    assert_eq!(note_name("fin."), "fin");
    assert_eq!(note_name("   "), "Nota");
    assert_eq!(note_name(&"x".repeat(80)).chars().count(), 60);
    assert_eq!(note_name("línea uno\nlínea dos"), "línea uno línea dos");
}

#[test]
fn notes_never_overwrite_another_note() {
    let folder = TempFolder::new("notes");
    let dir = folder.0.display().to_string();
    let first = save_note(dir.clone(), "TCP".into(), "uno".into(), None).unwrap();
    let second = save_note(dir.clone(), "TCP".into(), "dos".into(), None).unwrap();
    assert!(first.ends_with("TCP.md") && second.ends_with("TCP 2.md"));
    assert_eq!(fs::read_to_string(&first).unwrap(), "uno");
    assert_eq!(fs::read_to_string(&second).unwrap(), "dos");
}

#[test]
fn saving_again_updates_the_same_note() {
    let folder = TempFolder::new("update");
    let dir = folder.0.display().to_string();
    let first = save_note(dir.clone(), "TCP".into(), "uno".into(), None).unwrap();
    let again = save_note(dir, "TCP".into(), "uno y dos".into(), Some(first.clone())).unwrap();
    assert_eq!(again, first);
    assert_eq!(fs::read_to_string(&first).unwrap(), "uno y dos");
}

#[test]
fn a_note_outside_the_folder_is_never_rewritten() {
    let folder = TempFolder::new("outside");
    let other = TempFolder::new("outside-other");
    let foreign = other.0.join("ajeno.md");
    fs::write(&foreign, "no tocar").unwrap();
    let saved = save_note(folder.0.display().to_string(), "TCP".into(), "nota".into(), Some(foreign.display().to_string())).unwrap();
    assert!(saved.starts_with(&folder.0.display().to_string()));
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "no tocar");
}

#[test]
fn a_missing_notes_folder_is_an_error_and_is_not_created() {
    let folder = TempFolder::new("missing");
    let missing = folder.0.join("no-existe");
    let error = save_note(missing.display().to_string(), "x".into(), "y".into(), None).unwrap_err();
    assert!(error.contains("No encuentro la carpeta de notas"), "{error}");
    assert!(!missing.exists());
}

// --- History file -----------------------------------------------------------------------------

#[test]
fn history_starts_empty_and_is_replaced_whole() {
    let folder = TempFolder::new("history");
    let file = folder.0.join("history.json");
    assert_eq!(read_history_at(&file).unwrap(), "[]");
    write_history_at(&file, "[1]").unwrap();
    write_history_at(&file, "[1,2]").unwrap(); // Over an existing file: must replace it on Windows.
    assert_eq!(read_history_at(&file).unwrap(), "[1,2]");
    assert!(!file.with_extension("json.tmp").exists(), "no temporary file is left behind");
}

// --- OCR --------------------------------------------------------------------------------------

#[test]
fn ocr_rows_join_pieces_of_the_same_line_in_order() {
    let lines = vec![
        (80.0, 100.0, 20.0, "TypeError".to_string()),
        (130.0, 150.0, 300.0, "await fetchUser(id)".to_string()),
        (20.0, 40.0, 20.0, "Primera línea".to_string()),
        (131.0, 150.0, 20.0, "const user".to_string()),
    ];
    assert_eq!(rows(lines), "Primera línea\nTypeError\nconst user await fetchUser(id)");
}

#[test]
fn ocr_reads_text_and_code_from_an_image() {
    // Uses the OCR built into Windows with the user's display languages.
    let png = image::open(fixture("ocr-sample.png")).unwrap().to_rgba8();
    let (width, height) = png.dimensions();
    let image = arboard::ImageData { width: width as usize, height: height as usize, bytes: png.into_raw().into() };
    let text = recognize_text(&image).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].contains("protocolo TCP"), "{text}");
    assert!(lines[1].starts_with("TypeError"), "{text}");
    assert!(lines[2].starts_with("const user") && lines[2].contains("fetchUser(id)"), "{text}");
}

// --- Real models (ignored by default) ---------------------------------------------------------

/// The same `claude -p` call the app makes, with a tiny prompt. Uses the user's subscription.
#[test]
#[ignore]
fn real_claude_answers() {
    let command = claude_command("claude-haiku-5-5", "low", "Responde en una frase.");
    let answer = run_cli(&Answers::default(), 1, command, "¿Qué es TCP?", "Claude Code", |event, outcome| {
        on_claude_event(event, outcome, &mut |_| {})
    });
    println!("{answer:?}");
    assert!(answer.is_ok_and(|text| !text.trim().is_empty()));
}

/// The same `codex exec` call the app makes. Uses the user's ChatGPT plan.
#[test]
#[ignore]
fn real_codex_answers() {
    let command = codex_command("gpt-5.6-luna", "low").unwrap();
    let answer = run_cli(&Answers::default(), 1, command, "Responde en una frase: ¿qué es TCP?", "Codex", |event, outcome| {
        on_codex_event(event, outcome, &mut |_| {})
    });
    println!("{answer:?}");
    assert!(answer.is_ok_and(|text| !text.trim().is_empty()));
}

/// A real answer from the first local Ollama model, or `XPLANATOR_OLLAMA_MODEL`.
#[test]
#[ignore]
fn real_ollama_answers() {
    let model = std::env::var("XPLANATOR_OLLAMA_MODEL")
        .ok()
        .or_else(|| fetch_ollama_models()?.into_iter().find(|name| !name.contains("cloud")))
        .expect("Ollama running with a local model");
    let answers = Answers::default();
    answers.begin(1).unwrap();
    let messages = [ChatMessage { role: "user".into(), content: "Responde solo: TCP es un protocolo.".into() }];
    let answer = run_ollama(&answers, 1, &model, "Responde en una frase.", &messages, |_| {});
    println!("{model}: {answer:?}");
    assert!(answer.is_ok_and(|text| text.contains("TCP")));
    assert!(preload(&model).is_ok() && is_loaded(&model), "preload leaves the model in memory");
}
