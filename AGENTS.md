# Explicador: notas para agentes

App de bandeja para Windows (Tauri v2 + Rust + Vue 3 + TypeScript): seleccionas un texto, pulsas **Alt+X** y un recuadro lo explica en español, con repreguntas. El `README.md` cuenta cómo usarla e instalarla. Este fichero recoge lo que no se deduce del código: decisiones, porqués, datos comprobados y trampas.

Código, identificadores y commits en inglés; commits de una línea `tipo(explicador): frase`. Commit, push y PR solo con permiso del dueño, de uno en uno. Se trabaja en `main`.

## Mapa del código

- `src-tauri/src/lib.rs`: todo el lado nativo.
  - Atajo global y captura de la selección (`explain_selection`, `capture_selection`).
  - Registro de peticiones y cancelación (`Answers`).
  - Proveedores: `ask_claude_code`, `ask_codex`, `ask_ollama`.
  - Asistente de primera vez: `check_setup`, `open_ollama_download`, `start_ollama`, `pull_ollama_model`.
  - Bandeja, instancia única y arranque con Windows (`run`).
- `src/App.vue`: todo el frontend. Prompt (`SYSTEM_PROMPT`), selector de modelo, historial en memoria como texto plano y panel del asistente (`needsSetup`).
- Frontend y Rust se hablan con comandos de Tauri; el texto llega poco a poco por `Channel`.

## Decisiones y porqués

### Modelos (nada de APIs de pago)
- Claude por la suscripción del usuario con `claude -p`; Codex por la de ChatGPT con `codex exec`; Ollama gratis. El modo API de Anthropic se quitó a propósito.
- **`claude -p`:**
  - `--setting-sources ""`: con `project`, Claude Code sube por las carpetas padre y carga `~/.claude/CLAUDE.md` del usuario. Comprobado: unos 2.400 tokens de contexto con `project` frente a 909 sin fuentes.
  - `--tools ""`, `--strict-mcp-config`, `--disable-slash-commands`, `--no-session-persistence`.
  - **No usar `--bare`**: no lee el login OAuth y rompe la suscripción.
  - Se quitan `ANTHROPIC_API_KEY` y `ANTHROPIC_AUTH_TOKEN` del entorno; si no, `-p` cobraría la API.
  - Hace falta el `claude.exe` nativo: `Command::new` en Windows solo resuelve `.exe` (un `claude.cmd` de npm no arranca). Por eso la detección busca `claude.exe`.
- **`codex exec`:**
  - Se lanza con `cmd /d /c codex` para que también encuentre un `codex.cmd` (Volta, npm). Los argumentos han de ser palabras simples (`plain_word`), porque `cmd` interpretaría `&`, `|`…
  - Instrucciones y conversación van por stdin.
  - Flags: `--ignore-user-config --ignore-rules -s read-only --disable shell_tool --disable apps -c project_doc_max_bytes=0 -c web_search=disabled`. El texto seleccionado es entrada no fiable: sin shell ni conectores. Comprobado: pidiéndole `whoami` respondió "NO PUEDO" sin ejecutar nada.
  - Se quita `CODEX_API_KEY` del entorno.
  - Modelo `gpt-5.6-luna`: `gpt-6-luna` no está disponible con cuenta de ChatGPT.
  - No envía texto poco a poco, solo `item.completed`. Los eventos `error` son reintentos ("Reconnecting…") y no son fatales; solo `turn.failed` lo es.
  - Limitación: siempre lee el `~/.codex/AGENTS.md` global del usuario y no hay opción para evitarlo.
- **Ollama:**
  - Se llama desde Rust con `ureq`, no desde la ventana: Ollama rechaza el origen `tauri.localhost` (CORS).
  - El agente HTTP no usa el proxy del entorno y no convierte los errores HTTP en excepciones, para poder leer el texto de error de Ollama.
  - `think: false`, con un reintento sin ese campo si el error menciona "think".
  - Hay que recibir `done: true`; si no, la respuesta se considera cortada.
  - Solo se listan modelos con la capacidad `completion`.

### Cancelación (`Answers`)
- Los identificadores de petición siempre crecen (`Date.now()` en el frontend). Cancelar uno cancela también los anteriores, aunque aún no hayan empezado: una cancelación que llega antes de tiempo no se pierde.
- Se mata el árbol de procesos con `taskkill /T` (la cadena es cmd → volta → node → codex.exe).
- Ollama no se puede cortar antes del primer trozo de respuesta; está documentado en el README.

### Captura de la selección
- Espera a que se suelten los modificadores antes de simular Ctrl+C.
- `mask_alt_menu` pulsa una tecla sin asignar (0xE8) mientras Alt sigue apretado, para que soltar Alt no abra el menú de la app de origen. Es el truco de AutoHotkey. No se ha probado con teclas físicas.
- `CAPTURING` evita dos capturas a la vez. Solo se restaura texto plano en el portapapeles.

### App de escritorio
- **Atajo Alt+X.** Win+B y Win+X los reserva Windows (error 1409 al registrarlos). Ctrl+Shift+X choca con VS Code, Slack y Teams. Win+Shift+X era incómodo. Contrapartida: Word y Outlook pierden su Alt+X (convertir código Unicode).
- **Instancia única** (plugin registrado el primero). Relanzar con el recuadro visible solo le da el foco; con el recuadro oculto lo abre limpio.
- **"Iniciar con Windows":** casilla de la bandeja, desactivada por defecto. En las compilaciones de desarrollo está desactivada, porque registraría el ejecutable de desarrollo con el mismo nombre que el instalado.
- Los errores de arranque se muestran con `MessageBoxW`: la versión final no tiene consola.
- Las respuestas se pintan como texto plano (Vue escapa el HTML). No renderizar Markdown sin sanitizarlo.

### Asistente de primera vez
- Aparece solo si no hay ningún modelo utilizable.
- Recomienda `qwen3:4b-instruct` (2,5 GB) con 10 GB de RAM o más, y `qwen3:1.7b` (1,4 GB) con menos.
- Abre la descarga oficial de Ollama en el navegador; la app nunca descarga ni ejecuta instaladores.
- La selección que había pendiente se explica sola al terminar.
- El modelo guardado solo se sustituye si ha desaparecido de verdad. Con Ollama cerrado se conserva, para no pasar en silencio a una suscripción cuando Ollama arranca más tarde que la app.

## Desarrollar y probar

- `npm run tauri dev` para desarrollar. `npm run tauri build` genera el instalador en `src-tauri/target/release/bundle/nsis/Explicador_0.1.0_x64-setup.exe`; la compilación final tarda entre 3 y 8 minutos.
- La app instalada no se actualiza sola: tras cada cambio hay que recompilar y reinstalar.
- Antes de usar `tauri dev` o de probar el ejecutable final, cierra la app instalada. Comparten atajo e identificador, y la instancia única reenvía la segunda copia a la primera y la cierra.
- Con una ruta de proyecto muy larga, el enlazador de Windows falla (`LNK1104`). Usa una ruta corta o un `CARGO_TARGET_DIR` corto.
- No hay tests en el repo. Para comprobar con llamadas reales:
  1. Añade un módulo `#[cfg(test)]` temporal al final de `lib.rs` que llame a `run_cli`, `run_ollama` o `check_setup`.
  2. Ejecuta `cargo test --lib -- <filtro> --nocapture --test-threads 1`.
  3. Quítalo después, sin dejar líneas en blanco de más.
- Para probar la interfaz sin Tauri, usa un HTML aparte:
  1. Define un `window.__TAURI_INTERNALS__` falso que responda a los comandos.
  2. Carga `http://localhost:1420/src/main.ts` desde otro puerto, con `vite` en marcha.
  3. Ábrelo en el navegador interno.
- Revisión cruzada con la skill `codex-review`. Codex tiene límites de cuota y a veces no está disponible.
- El equipo del dueño no tiene GPU: `qwen3:30b-a3b` tarda unos 60 s en cargar y genera unos 4 tokens por segundo.

## Distribución y cuentas (investigado en noviembre de 2026)

- Anthropic prohíbe desde febrero de 2026 que apps de terceros ofrezcan el login de las suscripciones o usen sus tokens. Claude solo puede usarse con el Claude Code del propio usuario; repartir la app con eso como función principal es zona gris.
- OpenAI ofrece "Sign in with ChatGPT" oficial para apps de terceros (DevDay, 29-9-2026). Su documentación está pensada para apps de código abierto y locales. Usa el scope `chatgpt.tokens.use.direct` y la Responses API con `store: false` y `stream: true`.
- Firmar el instalador: Azure Artifact Signing solo admite particulares de EE. UU. y Canadá; un certificado OV cuesta entre 150 y 300 $ al año, y SmartScreen tarda semanas en confiar aun firmado.

## Pendiente

- Botones "Conectar" con login delegado (`claude auth login`, `codex login`) y detectar opencode.
- "Sign in with ChatGPT" oficial, para usar ChatGPT sin Codex.
- Actualizador automático (plugin `updater` de Tauri + GitHub Releases).
- Ideas para aprender, que se discuten en otra sesión: perfil "lo que ya sé", "Explícamelo tú" (técnica Feynman), estructura según el tipo de selección, botones de nivel, historial con repaso espaciado (Anki), explicación en cadena y OCR.
- Hallazgos menores sin corregir:
  - portapapeles ocupado por otra app en el momento de capturar;
  - falta un botón "Reintentar";
  - la primera ejecución puede elegir Claude si Ollama arranca más tarde que la app.
