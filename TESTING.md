# Pruebas de Xplainer

Criterios de aceptación de cada función y cómo se comprueba cada uno: con un test automático (su nombre entre comillas), con la prueba de humo del ejecutable final o a mano.

## Cómo ejecutarlas

```bash
npm test                     # frontend: App.vue con los comandos de Rust simulados (Vitest + jsdom)
cargo test --manifest-path src-tauri/Cargo.toml   # Rust: lógica sin ventana ni modelos
npm run test:all             # las dos anteriores
npm run build                # revisión de tipos (vue-tsc) y build del frontend
npm run tauri build          # ejecutable final e instalador
```

Llamadas a modelos reales (gastan cuota de tus suscripciones y tiempo; necesitan Claude Code, Codex y Ollama con un modelo local):

```bash
cargo test --manifest-path src-tauri/Cargo.toml -- --ignored --nocapture --test-threads 1
```

Los datos de `src-tauri/tests/fixtures` son salidas reales de `claude -p`, `codex exec`, Ollama y una imagen para el OCR, grabadas una vez y sin identificadores.

## Criterios

### Proveedores de modelos
- **P1.** La respuesta de Claude se lee del streaming de `claude -p` y el texto parcial suma la respuesta final. Test: "claude_stream_gives_partial_text_and_the_final_answer".
- **P2.** Un error de Claude se muestra con su mensaje. Test: "claude_error_result_becomes_a_readable_error", "claude_error_with_only_an_errors_list_is_reported".
- **P3.** Codex da su `agent_message`. Un turno fallido es un error, un aviso no es una respuesta y los reintentos solo se muestran si no llega respuesta. Test: "codex_stream_gives_the_agent_message", "codex_failed_turn_is_an_error_and_warnings_are_not_answers", "codex_retry_errors_are_shown_only_when_no_answer_arrives".
- **P4.** Una CLI que sale con error no da por buena una respuesta a medias, y una que no arranca se explica. Test: "an_answer_from_a_process_that_failed_is_not_trusted", "run_cli_explains_a_program_that_cannot_start", "run_cli_reads_a_real_process_and_returns_its_answer".
- **P5.** Ollama se lee hasta `done`; una respuesta cortada, vacía o con error se dice. Test: "ollama_stream_is_read_until_done", "ollama_stream_without_done_is_reported_as_cut", "ollama_stream_stops_when_cancelled_and_reports_errors".
- **P6.** Solo se listan modelos de Ollama que pueden conversar. Test: "only_models_that_can_chat_are_listed".
- **P7.** Los valores que van a la línea de comandos de Codex no pueden inyectar órdenes. Test: "plain_word_accepts_model_names_and_rejects_shell_characters".
- **P8.** Con modelos reales, Claude, Codex y Ollama responden con la configuración de la app. Tests ignorados: "real_claude_answers", "real_codex_answers", "real_ollama_answers".
- **P9.** Claude y Codex se lanzan aislados (sin herramientas, MCP, shell ni tu configuración) y sin las claves de API del entorno, para cobrar siempre de la suscripción; nunca con `--bare`. Test: "the_clis_run_isolated_and_bill_the_subscription".

### Cancelación y una respuesta a la vez
- **C1.** Una cancelación que llega antes que su petición cuenta, y cancela las anteriores. Test: "a_cancel_that_arrives_first_still_counts".
- **C2.** Una petición nueva sustituye a la anterior. Test: "a_newer_request_replaces_the_older_one".
- **C3.** Cancelar mata el proceso en marcha. Test: "cancelling_kills_the_running_process".
- **C4.** Una selección nueva cancela la respuesta en curso. Test de frontend: "cancels the answer in flight when a new selection arrives".

### Asistente de primera vez y elección de modelo
- **A1.** La primera vez se elige el modelo local gratuito, y Claude y Codex solo aparecen si están instalados. Test de frontend: "picks the free local model on first run…".
- **A2.** Sin ningún modelo aparece el asistente, no se pregunta a nadie y, cuando hay modelo, la selección pendiente se explica con su pista de ventana. Test de frontend: "shows the assistant when no model works…".
- **A3.** Con Ollama cerrado se conserva el modelo elegido. Test de frontend: "keeps a chosen Ollama model while Ollama is closed".
- **A4.** El modelo recomendado depende de la RAM. Test: "the_recommended_model_fits_the_memory".
- **A5.** Instalar y abrir Ollama y descargar el modelo con barra de progreso. A mano, en un equipo sin Ollama.

### 1 · Explicación según el tipo de texto
- **E1.** El prompt lleva una estructura para términos, párrafos, código y errores, y la selección va con la ventana como pista. Test de frontend: "sends the selection with the window as a hint and the structured prompt".
- **E2.** Que el modelo respete cada estructura es comportamiento del modelo: se comprobó con llamadas reales a Haiku (término, párrafo, código y error), no con un test fijo.
- **E3.** Las preguntas sugeridas se muestran y repreguntan con toda la conversación. Test de frontend: "asks a suggested question with the whole conversation".
- **E4.** El texto aparece según llega y al final se sustituye por la respuesta limpia; los trozos que llegan tarde se ignoran. Test de frontend: "shows the text as it streams in, then the clean final answer, and ignores late chunks".

### 3 · Más simple, Más técnico, Otro significado
- **R1.** Los botones aparecen bajo la última respuesta, mandan la instrucción completa y muestran solo su nombre; "Otro significado" solo con textos cortos. Test de frontend: "offers simpler and more technical versions…".

### 2 · Precarga de Ollama
- **O1.** Elegir un modelo local lo precarga. Test de frontend: "picks the free local model on first run…".
- **O2.** "Cargando el modelo en memoria…" solo si el modelo no está cargado. Test de frontend: "says the local model is loading, and only then"; Rust: "a_loaded_model_is_recognised_in_api_ps".
- **O3.** La precarga deja el modelo en memoria. Test ignorado: "real_ollama_answers".

### 10 · Explicación en cadena
- **K1.** Una palabra de la respuesta se explica como un nivel más, con su contexto, y el rastro permite volver. Test de frontend: "explains a word from the answer one level deeper and goes back".
- **K2.** Seleccionar una palabra muestra el botón "Explicar «…»". Test de frontend: "offers the selected word with a button next to it".
- **K3.** No se baja de nivel mientras llega una respuesta. Test de frontend: "does not go deeper while an answer is still arriving".
- **K4.** Alt+X dentro del recuadro baja un nivel sin mover el recuadro. A mano (teclas físicas).

### 5 · Guardar nota
- **N1.** Sin carpeta, "Guardar nota" lleva a Ajustes; con carpeta guarda Markdown con propiedades y, tras repreguntar, actualiza la misma nota. Test de frontend: "asks for the folder first…".
- **N2.** Los nombres de fichero son válidos en Windows. Test: "note_names_are_valid_windows_file_names".
- **N3.** Nunca se sobrescribe otra nota ni un fichero fuera de la carpeta, y una carpeta que no existe no se crea. Test: "notes_never_overwrite_another_note", "saving_again_updates_the_same_note", "a_note_outside_the_folder_is_never_rewritten", "a_missing_notes_folder_is_an_error_and_is_not_created".

### 4 · Historial
- **H1.** Cada conversación se guarda con su ventana, la más reciente primero, y se actualiza al repreguntar. Test de frontend: "saves each conversation with its window…".
- **H2.** No se guarda nada de ventanas excluidas, por palabra completa. Test de frontend: "never saves text from excluded windows, matching whole words".
- **H3.** En pausa no se guarda nada nuevo. Test de frontend: "saves nothing new while paused".
- **H4.** La búsqueda ignora mayúsculas y acentos, y una entrada se reabre para seguir preguntando. Test de frontend: "finds entries without caring about accents or case, and reopens them".
- **H5.** Se borra una entrada, y todo solo tras un segundo clic. Test de frontend: "deletes one entry, and everything only after a second click".
- **H6.** El fichero se sustituye entero, sin dejar temporales, y no se reescribe si no se pudo leer. Test: "history_starts_empty_and_is_replaced_whole"; frontend: "never overwrites a history file it could not read".

### 6 · OCR con recorte (Win+Shift+Z)
- **R2.** El OCR de Windows lee texto y código de una imagen, con las líneas en orden. Test: "ocr_reads_text_and_code_from_an_image", "ocr_rows_join_pieces_of_the_same_line_in_order".
- **R3.** Un recorte sin texto abre el recuadro con un aviso. Test de frontend: "opens empty with a message when a snip has no text…".
- **R4.** Win+Shift+Z queda registrado. Prueba de humo.
- **R5.** Recortar con la herramienta de Windows y cancelar con Esc. A mano.

### App de escritorio
- **D1.** Alt+X queda registrado al arrancar. Prueba de humo.
- **D2.** Una sola instancia: abrirla otra vez no crea otra copia y muestra el recuadro. Prueba de humo.
- **D3.** "Iniciar con Windows" viene desactivado. Prueba de humo.
- **D4.** Esc oculta el recuadro. Test de frontend: "opens empty with a message when a snip has no text, and hides on Esc".
- **D5.** Alt+X copia la selección de otra app y deja el portapapeles como estaba; el menú de la app de origen no se abre. A mano (teclas físicas).
- **D6.** Icono de la X en la bandeja y menú (Iniciar con Windows, Salir). A mano.

## Prueba de humo del ejecutable final

Con `src-tauri/target/release/xplainer.exe` y sin otra copia abierta (ni Xplanator ni Explicador, que comparten Alt+X):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/smoke-test.ps1
```

Escribe `PASS` o `FAIL` por cada comprobación y termina con código 1 si alguna falla:

1. Arrancarlo y comprobar que Alt+X (`MOD_ALT`, `X`) y Win+Shift+Z (`MOD_WIN | MOD_SHIFT`, `Z`) están ocupados: `RegisterHotKey` desde otro proceso falla con el error 1409.
2. Arrancarlo otra vez: la segunda copia termina sola, queda un solo proceso y el recuadro, que empezó oculto, se hace visible.
3. Comprobar que `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` no tiene una entrada de Xplainer.
4. Cerrarlo y comprobar que los atajos vuelven a quedar libres.
