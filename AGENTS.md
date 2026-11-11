# Xplainer: notas para agentes

App de bandeja para Windows y Linux (Tauri v2 + Rust + Vue 3 + TypeScript): seleccionas un texto, pulsas **Alt+X** y un recuadro lo explica en español, con repreguntas. **Win+Shift+Z** hace lo mismo con un recorte de pantalla y OCR (solo Windows). Antes se llamaba Xplanator y, antes aún, Explicador; el identificador cambió a `com.kiook.xplainer`, así que es otra app para Windows (sin el historial ni los ajustes de las anteriores). El `README.md` cuenta cómo usarla e instalarla. Este fichero recoge lo que no se deduce del código: decisiones, porqués, datos comprobados y trampas.

Código, identificadores y commits en inglés; commits de una línea `tipo(xplainer): frase`. Commit, push y PR solo con permiso del dueño, de uno en uno. Se trabaja en `main`.

## Mapa del código

- `src-tauri/src/lib.rs`: todo el lado nativo.
  - Atajo global y captura de la selección (`explain_selection`, `capture_selection`).
  - Registro de peticiones y cancelación (`Answers`).
  - Proveedores: `ask_claude_code`, `ask_codex`, `ask_ollama`.
  - Primera vez: `check_setup`, `check_accounts`, `sign_in`, `open_install_page`, `open_ollama_download`, `start_ollama`, `pull_ollama_model`.
  - Precarga de Ollama: `preload_ollama`, `ollama_loaded`.
  - Alt+X dentro del recuadro (`popup_is_foreground`) emite `explain-deeper` en vez de `explain`.
  - Recorte con OCR: `snip_screen`, `recognize_text`, `rows`, `explain_snip`.
  - Ficheros: `save_note`, `note_name`, `read_history`, `write_history`.
  - Bandeja, instancia única y arranque con Windows (`run`).
  - Lo que solo existe en un sistema lleva `#[cfg(windows)]` o `#[cfg(target_os = "linux")]`. Lo común entre ambos: `background` (proceso hijo sin consola o en su propio grupo), `kill_tree`, `codex`, `open_url`, `explain_key`, `shortcuts`.
- `src-tauri/src/linux.rs`: lo que Linux hace distinto (selección primaria, PATH de la sesión, terminal para el login, Ollama, diálogo de error).
- `src-tauri/src/screen_context.rs`: lo que rodea la selección, leído al pulsar Alt+X (`start`, `finish`, `page_address`). En Windows con UI Automation; en Linux con AT-SPI, en `screen_context/linux.rs`, que además da el título y el texto seleccionado.
- `src-tauri/tauri.linux.conf.json`: en Linux, Tauri lo combina con `tauri.conf.json` y genera `.deb` y AppImage en vez de NSIS.
- `src/App.vue`: todo el frontend.
  - Prompt (`SYSTEM_PROMPT`), con una estructura por tipo de texto: término, párrafo, código o error.
  - Bloque `<contexto>` de la primera pregunta (`contextLines`, `sentFirstQuestion`, `firstPrompt`).
  - Selector de modelo y panel del asistente (`needsSetup`).
  - Conversación en memoria como texto plano (`history`); niveles de la cadena en `trail`.
  - Vistas `chat`, `history` y `settings`. Los ajustes van en `localStorage`.
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

### Primera vez: elegir la IA
- La primera vez (sin `xplainer.onboarded` en `localStorage`) el recuadro pregunta con qué IA explicar: Claude, ChatGPT (Codex) o gratis con Ollama. Hasta elegir no se pregunta ni se precarga nada. Se puede volver a abrir en Ajustes → "Elegir otra IA".
- "Vincular" es el login del propio CLI (`claude auth login --claudeai`, `codex login`) en una consola propia (`CREATE_NEW_CONSOLE`): Xplainer nunca ve la cuenta. `|| pause` deja la ventana abierta solo si falla.
- Sesión detectada con `claude auth status --json` (`loggedIn` y `authMethod == "claude.ai"`) y `codex login status` ("Logged in using ChatGPT", en stderr). Una cuenta de Console o una clave de API cobraría la API, así que no cuentan. Cada comprobación se abandona a los 10 s: `claude` puede esperar a la red para renovar el login.
- Tras pulsar "Iniciar sesión" se comprueba cada 3 s; al detectarla se elige sola. Elegir otra cosa olvida el login pendiente, y una descarga de Ollama empezada antes de otra elección no la pisa (`choiceRound`).
- Sin ningún modelo utilizable, fuera de la primera vez, aparece el asistente de Ollama.
- Recomienda `qwen3:4b-instruct` (2,5 GB) con 10 GB de RAM o más, y `qwen3:1.7b` (1,4 GB) con menos.
- Abre la descarga oficial de Ollama en el navegador; la app nunca descarga ni ejecuta instaladores.
- La selección que había pendiente se explica sola al terminar.
- El modelo guardado solo se sustituye si ha desaparecido de verdad. Con Ollama cerrado se conserva, para no pasar en silencio a una suscripción cuando Ollama arranca más tarde que la app.

### Funciones para aprender (v2)
- **Prompt por tipo de texto.** Haiku con esfuerzo bajo tiende a anunciar "es un término, así que…". Pedirle que clasifique "en silencio", decirle que la respuesta empieza por el punto 1 y meter la regla de "di qué significado elegiste" dentro del punto 1 de los términos lo reduce, pero no lo elimina: medido con llamadas reales en noviembre de 2026, aún sale en 1 de cada 3 o 4 respuestas a un término suelto; con código, párrafos y errores no salió. A veces tampoco numera los puntos.
- **Precarga de Ollama.** Al elegir un modelo local se manda un `/api/chat` con `messages: []`. `keep_alive` de 30 min, también en cada respuesta (el de Ollama por defecto es de 5). Los modelos `cloud` no se precargan. "Cargando el modelo…" sale de `/api/ps`, consultado antes de marcar la petición en curso.
- **Cadena.** No se puede bajar de nivel mientras se responde: el nivel guardado quedaría con media respuesta. Cada nivel es una conversación aparte: entrada propia en el historial, nota propia.
- **Notas.** La carpeta ha de existir; no se crea, para que una errata no deje carpetas sueltas. Nunca se sobrescribe otra nota. `existing` solo se reescribe si está dentro de la carpeta elegida. Windows 11 ya no reserva `CON.ext` y similares, solo `NUL`; los nombres exactos se protegen igualmente.
- **Historial.** JSON en `app_data_dir`, escrito a un `.tmp` y renombrado: `fs::rename` sustituye en Windows (`MOVEFILE_REPLACE_EXISTING`). Los comandos síncronos de Tauri corren en el hilo principal, así que las escrituras no se pisan. No se escribe hasta haber leído el fichero, para no borrarlo si la lectura falla. La exclusión compara palabras completas del título de la ventana: con subcadenas, "ING" bloquearía "Settings".
- **OCR.** Abre `ms-screenclip:` y espera una imagen nueva en el portapapeles (30 s). Cada Win+Shift+Z tiene un número y la espera antigua se rinde ante una nueva: si no, cancelar con Esc bloqueaba el atajo. La imagen se queda en el portapapeles, como con Win+Shift+S. El OCR de Windows parte las líneas de código donde hay huecos y las desordena: `rows` las vuelve a juntar por altura.

### Contexto de la ventana
- **Por qué.** Una palabra suelta es ambigua: "Negotiations", seleccionada en la vista de negociaciones de un espacio de datos, se explicaba como regatear un coche. Con el título de la ventana no basta. Medido con Haiku en noviembre de 2026: con el título, 2 de 2 respuestas genéricas; con la URL y el texto de alrededor, 2 de 2 en el ámbito correcto.
- **Cómo.** Al pulsar Alt+X, Rust lee la ventana de origen con UI Automation en un hilo propio, mientras se copia la selección. Envía la app, la URL y unas 8 líneas antes y después de la selección. Si no hay texto seleccionable, envía los nombres de los elementos cercanos al cursor.
- **Límites de lo que se lee.**
  - Solo dentro de una página o documento: nunca la barra de direcciones ni las pestañas.
  - Nunca se sube hasta el documento entero, que empezaría por sus menús.
  - El cursor solo cuenta sobre la misma ventana: un navegador muestra muchas desde un solo proceso.
  - Como mucho 1.500 caracteres y 200 nombres.
- **Tiempos.** `CUIAutomation8` con 700 ms por llamada (el cliente antiguo no corta con una app colgada) y 800 ms en total; si no llega, se explica sin contexto. Mientras siga abierta una lectura no empieza otra, para no acumular hilos.
- **Chromium despierta tarde.** Chrome, Edge y las apps Electron (clase de ventana `Chrome_WidgetWin_*`) solo construyen su árbol de accesibilidad cuando un cliente lo pide por primera vez. Esa primera respuesta es un panel vacío, así que el primer Alt+X tras abrir Chrome se quedaba sin contexto. Con esas ventanas, si no hay documento, se pregunta otra vez a los 300 ms. Medido con Chrome recién abierto: página encontrada a los 390 ms, contexto enviado a los 399 ms. Ojo al probarlo: cerrar las ventanas no basta, Chrome sigue vivo en segundo plano; hay que salir con `chrome://quit`.
- **URL.** Solo `http(s)`, sin `usuario:clave@`, sin `?query` ni `#fragment`. De la ruta solo quedan los tramos que son palabras en minúscula; el resto pasa a `…`, porque puede ser un id o un token. Los códigos de letras con guiones (como los de Google Meet) parecen palabras y pasan.
- **Privacidad.** Se activa por defecto, con la casilla "Usar el contexto de la ventana". La lista de exclusión del historial también bloquea el contexto. Ambos ajustes se aplican justo al enviar, no al capturar. El contexto solo viaja en la primera pregunta: la conversación guarda la versión sin él, así que no llega en repreguntas, ni a otro modelo elegido a mitad, ni al historial.
- **Entrada no fiable.** `<` y `>` del contexto pasan a `‹` y `›`: el texto de la página no puede cerrar `<contexto>`. El prompt dice que el contexto solo sirve para elegir el significado y que no se siguen sus instrucciones.
- **Comprobado a mano** en Chrome, con la vista real de negociaciones (`localhost`). Sin probar: Edge, VS Code, Office y una app colgada de verdad.

### Linux (v3)
- **Alcance elegido por el dueño:** el núcleo sin OCR. Atajo en X11 y orden `xplainer --explain` en Wayland; título y contexto como en Windows; `.deb` y AppImage.
- **Wayland no deja registrar atajos globales** (`global-hotkey` solo funciona en X11). Con `WAYLAND_DISPLAY` o `XDG_SESSION_TYPE=wayland` no se registra Alt+X: el usuario crea en su escritorio un atajo que ejecute `xplainer --explain`, y la instancia única le pasa la orden a la copia en marcha. Si la app estaba cerrada, la copia nueva explica la selección cuando la interfaz avisa con `listening` de que ya escucha `explain`; un aviso anterior se perdería. El portal `GlobalShortcuts` (GNOME 48+, KDE) sería la mejora; no se hizo para no añadir `ashpd` ni un `.desktop` registrado.
- **No se simula Ctrl+C.** En Wayland no se puede, y en X11 no hace falta. La selección sale de:
  - AT-SPI, que dice qué está seleccionado ahora. En una página web solo da el trozo de un párrafo.
  - La selección primaria de X11 (`arboard`), que trae la selección entera pero puede ser antigua (un token seleccionado hace una hora en la terminal).
  - AT-SPI responde una de tres cosas (`Selected`): un texto, "nada seleccionado" o "no lo sé" (la app no tiene objeto de texto o no está en el bus).
  - Con un texto, gana la primaria si lo contiene, comparando palabras y no saltos de línea. Con "nada seleccionado" no se envía nada. Solo con "no lo sé" se usa la primaria sola (`current_selection`).
  - En GNOME con Wayland una app en segundo plano no puede leer la primaria de Wayland (Mutter no tiene `ext-data-control`); KDE y Sway sí, con la función `wayland-data-control` de `arboard`. Sin ese protocolo, `arboard` lee la de XWayland. Sin comprobar si Mutter la mantiene al día.
- **Alt+X dentro del recuadro** emite `explain-deeper` sin texto, y la interfaz lee su propia selección (`window.getSelection()`): Rust no puede leerla en Wayland.
- **AT-SPI sin escuchar eventos.** En cada lectura:
  1. `org.a11y.Bus.GetAddress` en el bus de sesión da el bus de accesibilidad.
  2. Del registro, la ventana con estado `Active`; su nombre es el título.
  3. `Collection.GetMatches` con `Focused` da el objeto enfocado.
  4. Se sube hasta un documento (la URL sale de `DocURL` en Firefox o `URI` en Chromium).
  5. Se baja por los U+FFFC (objetos incrustados) hasta el nodo con la selección, como hace Orca.
  6. Si la selección tiene texto y además U+FFFC (un enlace dentro de una frase), cada U+FFFC se sustituye por el texto de su objeto; si no, la frase perdería palabras.
  - Llamadas bloqueantes de zbus con 700 ms cada una, también en el bus de sesión, y los bucles miran el límite de 800 ms de Windows.
- **Trampa de `GetMatches`:** el constructor de `ObjectMatchRule` deja como `Invalid` el tipo de coincidencia de lo que no se rellena, y GTK entonces no devuelve nada (ni con la regla vacía). Hay que poner `NA` en atributos, roles e interfaces. Comprobado con una ventana GTK3.
- **Chrome, Firefox y Electron** solo publican su árbol si la accesibilidad del escritorio está activa (variables `ACCESSIBILITY_ENABLED`/`GNOME_ACCESSIBILITY`, `org.a11y.Status.IsEnabled` o `toolkit-accessibility` en gsettings). Xplainer no la activa: es un ajuste global, persistente y que gasta memoria en todas las apps. Sin ella no hay contexto ni selección exacta en el navegador; el título de la ventana sí suele llegar.
- **Solo dentro de un documento:** el texto de alrededor solo se envía si hay un documento por encima o el objeto es de varias líneas (editor, terminal). Así la barra de direcciones, que es de una línea, nunca viaja.
- **PATH.** Una app abierta desde el escritorio no hereda lo que añaden `~/.bashrc` o `~/.profile` (nvm, Volta, `~/.local/bin`). Al arrancar se lee el PATH de `$SHELL -ilc` (3 s como mucho) para encontrar `claude` y `codex`. Va entre dos marcas, porque los ficheros de arranque y `~/.bash_logout` pueden imprimir antes y después.
  - Con `--explain` y otra copia en marcha no se lee: esa orden solo pasa el aviso y el atajo no debe esperar al shell. La copia en marcha se detecta por el nombre que la instancia única tiene en el bus de sesión (identificador + `.SingleInstance`); un test comprueba que sigue al identificador.
  - Los programas se buscan en el PATH desde Rust, sin `which`, que falta en algunas distribuciones mínimas.
- **Procesos.** Cada hijo va en su propio grupo (`process_group(0)`), y cancelar manda `SIGKILL` al grupo entero: así muere también el nieto (sh → node → codex). Lo cubre un test.
- **Login de los CLIs** en una terminal, la primera que arranque: `x-terminal-emulator`, `ptyxis`, `gnome-terminal`, `kgx`, `konsole`, `xfce4-terminal`, `kitty`, `alacritty`, `foot` o `xterm`. Ptyxis y GNOME Console reciben la orden tras `--`, según sus páginas de manual.
- **Comprobado en WSLg (Ubuntu 24.04)** con una ventana GTK3:
  - Por AT-SPI: título, "Negotiations" seleccionada y sus líneas de alrededor, en 12 ms.
  - Alt+X simulado con `xdotool` en X11, y `xplainer --explain` en modo Wayland: el recuadro abre con la palabra.
  - Se generan el `.deb` y el AppImage.
  - Sin probar: un escritorio GNOME o KDE real, Chrome y Firefox con la accesibilidad activa, la bandeja (WSLg no tiene) y "Iniciar con la sesión" desde el AppImage.
- **Probar en WSLg:**
  - Envuelve app y ventana de prueba en `dbus-run-session` para que compartan el bus de accesibilidad.
  - Usa `GDK_BACKEND=x11` para poder hacer capturas con `import -window <id>`. La pantalla entera no se puede capturar.
  - Para el modo X11, quita `WAYLAND_DISPLAY`.
  - Una ventana nueva no recibe el foco si en Windows estás usando otra app, y entonces no está `Active`.

## Desarrollar y probar

- `npm run tauri dev` para desarrollar. `npm run tauri build` genera el instalador en `src-tauri/target/release/bundle/nsis/Xplainer_0.2.0_x64-setup.exe`; la compilación final tarda entre 3 y 8 minutos.
- La app instalada no se actualiza sola: tras cada cambio hay que recompilar y reinstalar.
- Antes de usar `tauri dev` o de probar el ejecutable final, cierra la app instalada. Comparten atajo e identificador, y la instancia única reenvía la segunda copia a la primera y la cierra.
- Con una ruta de proyecto muy larga, el enlazador de Windows falla (`LNK1104`). Usa una ruta corta o un `CARGO_TARGET_DIR` corto.
- **Linux desde Windows, con WSL:**
  1. Instala las librerías de Tauri (lista oficial de prerrequisitos) y `xdotool` para simular teclas.
  2. Copia el proyecto al disco de Linux (`rsync` sin `target`, `node_modules` ni `.git`): compilar desde `/mnt/c` es muy lento.
  3. La interfaz se genera en Windows con `npm run build`, porque el Node de Ubuntu 24.04 (18) es demasiado viejo para Vite 8.
  4. En Linux, `cargo test` y `tauri build --config '{"build":{"beforeBuildCommand":""}}'`.
  5. El AppImage necesita dos cosas en WSL. Primero, `APPIMAGE_EXTRACT_AND_RUN=1`, porque no hay FUSE y `linuxdeploy` también es un AppImage. Segundo, un PATH solo de Linux: el WSL añade carpetas de Windows y `linuxdeploy` aborta al no poder leer una (`Permission denied`).
- Tests: `npm run test:all` (Vitest para `App.vue` con los comandos de Rust simulados, y `cargo test` para `src-tauri/src/tests.rs`). Los que llaman a modelos reales están marcados `#[ignore]`. La prueba del ejecutable final es `scripts/smoke-test.ps1`. `TESTING.md` recoge los criterios de aceptación y qué test cubre cada uno.
- En PowerShell, `$null` pasado a un parámetro `string` de una función nativa llega como cadena vacía: `FindWindow($null, ...)` no encuentra nada. Por eso la prueba del ejecutable recorre las ventanas con `EnumWindows`.
- Para ver la interfaz sin Tauri (capturas, por ejemplo), usa un HTML aparte:
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

- Detectar opencode como otra opción de la primera vez.
- "Sign in with ChatGPT" oficial, para usar ChatGPT sin Codex.
- Actualizador automático (plugin `updater` de Tauri + GitHub Releases).
- Ideas para aprender que el dueño no eligió en la v2: perfil "lo que ya sé", "Explícamelo tú" (técnica Feynman) y repaso espaciado enviando tarjetas a Anki (AnkiConnect).
- `App.vue` pasa de 1.100 líneas: historial y ajustes podrían ir en componentes propios.
- Hallazgos menores sin corregir:
  - portapapeles ocupado por otra app en el momento de capturar;
  - falta un botón "Reintentar".
