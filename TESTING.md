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
- **A0.** La primera vez se pregunta con qué IA explicar (Claude, ChatGPT o gratis con Ollama), sin preguntar ni precargar nada antes; la selección pendiente se explica al elegir, y la elección se recuerda. Test de frontend: "asks which AI to use first…".
- **A0b.** Cada opción dice qué le falta: iniciar sesión abre el login del propio CLI y se usa sola al detectarlo; si no está instalada, abre la guía oficial; la local guía la instalación de Ollama. Tests de frontend: "opens the subscription's own login…", "sends to the install guide…", "guides the free local setup…", "offers to sign in again…", "forgets a pending login…", "keeps a subscription chosen while the local model was still downloading", "ignores a download started before choosing again", "checks the computer again when the first check failed", "can be shown again from the settings".
- **A0c.** Solo cuenta como sesión iniciada la de una suscripción, no una clave de API ni una cuenta de Console, y una comprobación colgada se abandona a los 10 s. Tests: "only_a_subscription_login_counts_as_signed_in", "a_status_command_that_hangs_is_given_up"; real: "real_accounts_are_detected" (ignorado).
- **A0d.** El login real en una ventana de consola, con el navegador. A mano.
- **A1.** Sin modelo guardado se elige el modelo local gratuito, y Claude y Codex solo aparecen si están instalados. Test de frontend: "picks the free local model when none is chosen…".
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
- **O1.** Elegir un modelo local lo precarga. Test de frontend: "picks the free local model when none is chosen…".
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

### 11 · Contexto de la ventana
- **W1.** La primera pregunta lleva un bloque `<contexto>` con la app, la ventana, la página y el texto de alrededor; «Contexto» muestra lo enviado y "Repetir sin contexto" pregunta otra vez sin él. Test de frontend: "sends what surrounds the selection, shows it and can ask again without it".
- **W2.** Con la casilla quitada o en una ventana excluida, el texto de la ventana no sale del equipo, también si el ajuste cambia antes de enviar. Test de frontend: "keeps the window's text on this computer in excluded windows or with the setting off", "applies the settings when a selection waiting for setup is finally sent", "applies a setting changed while the local model is being checked".
- **W3.** El texto de la ventana solo va en la primera pregunta: ni en las repreguntas ni en el fichero del historial. Test de frontend: "sends the window's text only once: not in follow-ups nor in the history file".
- **W4.** El texto de otra app no puede cerrar el bloque `<contexto>`. Test de frontend: "does not let the window's text close the context block".
- **W5.** De la URL solo quedan el esquema, el host y los tramos de ruta que son palabras; nunca credenciales, `?query`, `#fragment`, ids ni tokens. Test: "page_address_keeps_only_scheme_host_and_path_of_web_pages".
- **W6.** El texto de alrededor no lleva iconos ni huecos y mantiene la selección en el centro; los nombres no se repiten y se cortan a 1.500 caracteres. Test: "clean_around_drops_icons_and_keeps_the_selection_in_the_middle", "clean_names_skips_empty_and_repeated_neighbours".
- **W7.** UI Automation lee la URL y el texto de alrededor de una página real en Chrome. A mano (comprobado con la vista de negociaciones del espacio de datos). Sin probar: Edge, VS Code, Office y una app colgada.

### App de escritorio
- **D1.** Alt+X queda registrado al arrancar. Prueba de humo.
- **D2.** Una sola instancia: abrirla otra vez no crea otra copia y muestra el recuadro. Prueba de humo.
- **D3.** "Iniciar con Windows" viene desactivado. Prueba de humo.
- **D4.** Esc oculta el recuadro. Test de frontend: "opens empty with a message when a snip has no text, and hides on Esc".
- **D5.** Alt+X copia la selección de otra app y deja el portapapeles como estaba; el menú de la app de origen no se abre. A mano (teclas físicas).
- **D6.** Icono de la X en la bandeja y menú (Iniciar con Windows, Salir). A mano.

### 12 · Linux
Los tests de Rust se pasan en Linux con `cargo test`; los marcados `#[cfg(windows)]` (OCR, `taskkill`) no se compilan allí.
- **L1.** Todo el lado nativo compila y pasa sus tests en Linux; en Windows no cambia nada. `cargo test` en los dos sistemas.
- **L2.** Cancelar mata el CLI y lo que este haya lanzado (su grupo de procesos). Test: "cancelling_kills_the_running_process_and_its_children".
- **L3.** Se explica la selección completa de la primaria solo si contiene lo que AT-SPI ve seleccionado ahora (aunque cambien los saltos de línea). Si la app dice que no hay nada seleccionado, una primaria antigua no se envía; sola, solo se usa si la app no puede decir nada. Test: "linux_uses_the_whole_primary_selection_only_when_it_is_current", y a mano en WSLg con una primaria antigua y la ventana GTK sin selección.
- **L3b.** Un enlace dentro de la selección no pierde sus palabras: cada U+FFFC se sustituye por el texto de su objeto. Sin probar: hace falta un navegador con la accesibilidad activa.
- **L3d.** `xplainer --explain` con la app cerrada la abre y explica la selección, en cuanto la interfaz escucha. Test de frontend: "explains the selection that opened the app (xplainer --explain with the app closed)"; falla si el aviso `listening` va antes de escuchar.
- **L3c.** `xplainer --explain` con otra copia en marcha no lee el PATH del shell; los programas se buscan sin `which`. Tests: "linux_single_instance_name_follows_the_identifier" y "linux_finds_programs_on_the_path_without_which".
- **L4.** AT-SPI lee el título, la selección y sus líneas de alrededor de la ventana activa. Test ignorado "real_linux_window_is_read", con una ventana GTK abierta (comprobado en WSLg).
- **L5.** En Wayland la interfaz no nombra atajos que no hay y explica cómo crear uno con `xplainer --explain`. Test de frontend: "on Wayland names no shortcut it lacks and says how to bind one".
- **L6.** Alt+X dentro del recuadro sin texto de Rust explica la selección del propio recuadro. Test de frontend: "reads its own selection when the shortcut brings no text (Linux)".
- **L7.** Alt+X en X11 y `xplainer --explain` en Wayland abren el recuadro con la palabra seleccionada. A mano en WSLg, con `xdotool`.
- **L8.** Sin probar: un escritorio GNOME o KDE real, la bandeja, "Iniciar con la sesión" desde el AppImage, y Chrome o Firefox con la accesibilidad activa.

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
