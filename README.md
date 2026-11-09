# Xplanator

Selecciona un texto en cualquier app de Windows, pulsa **Alt+X** y aparece un recuadro con una explicación breve y didáctica del término. Puedes repreguntar escribiendo o pulsando una de las preguntas sugeridas.

Si el texto no se puede seleccionar (en una imagen, un vídeo, un PDF bloqueado), pulsa **Win+Shift+Z**, recorta la zona con la herramienta de recortes de Windows y Xplanator lee el texto con el OCR de Windows y lo explica.

Hecho con Tauri v2 (Rust) + Vue 3 + TypeScript.

## Modelos

Se eligen en el desplegable de la cabecera del recuadro, que recuerda la última elección. Ninguno cobra por uso:

- **Claude Haiku · suscripción**: tu plan de Claude a través de Claude Code (`claude -p`). Gasta de tus límites de uso. Necesita Claude Code instalado con el instalador nativo (`claude.exe` en el PATH; la instalación por npm no sirve) y con la sesión iniciada.
- **Codex GPT-5.6 Luna · suscripción**: tu plan de ChatGPT a través de Codex (`codex exec`). Gasta de tus límites de Codex. La respuesta aparece entera al final, no poco a poco. Necesita Codex con la sesión de ChatGPT iniciada.
- **Modelos de Ollama · gratis**: el desplegable lista los que tengas instalados (se refresca al abrirlo). Sin GPU dedicada, los modelos grandes van lentos; uno pequeño responde antes. Al elegir un modelo local en el desplegable, Xplanator lo carga en memoria en ese momento y lo mantiene 30 minutos desde su último uso (Ollama, por defecto, 5), para que no esperes a la carga al preguntar; mientras, ocupa RAM. Si aún se está cargando, el recuadro dice «Cargando el modelo en memoria…». Los modelos de la nube de Ollama (por ejemplo, los grandes de DeepSeek o Kimi, que no caben en un portátil) aparecen igual si los añades con `ollama pull` y has iniciado sesión en Ollama; su plan gratuito tiene límites.

## Requisitos

- Node.js y Rust (`rustup`, toolchain `stable-msvc`).
- Build Tools de Visual Studio con C++ y el SDK de Windows.

## Instalar

```bash
npm install
npm run tauri build
```

Genera el instalador en `src-tauri/target/release/bundle/nsis/Xplanator_0.2.0_x64-setup.exe`. Se instala para tu usuario, sin permisos de administrador, y queda en el menú Inicio. Como el instalador no está firmado, Windows SmartScreen avisa la primera vez: **Más información → Ejecutar de todas formas**.

Para que arranque sola al iniciar sesión: clic derecho en el icono de la bandeja → **Iniciar con Windows**. Viene desactivado; tú decides. Para desinstalarla: Configuración de Windows → Aplicaciones.

Xplanator es el nuevo nombre de Explicador. Windows las trata como apps distintas y las dos usan el mismo atajo, así que desinstala Explicador antes de instalar Xplanator.

## Usar

La app vive oculta, con un icono en la bandeja del sistema. Si la abres otra vez desde el menú Inicio, no arranca una segunda copia: abre el recuadro de la que ya está en marcha. Para salir: clic derecho en el icono → **Salir**.

En el recuadro: **Esc** lo oculta y la cabecera sirve para arrastrarlo.

Bajo cada respuesta, **Más simple** y **Más técnico** cambian el nivel de la explicación, y **Otro significado** (solo con palabras o expresiones cortas) pide la siguiente acepción.

**Guardar nota** crea un `.md` con la conversación en tu carpeta de notas, por ejemplo una carpeta de tu vault de Obsidian. La carpeta se elige en **⚙ Ajustes** y tiene que existir. Nunca sobrescribe otra nota: si guardas otra vez tras repreguntar, actualiza la misma.

**🕘 Historial** guarda cada conversación para buscarla (sin distinguir mayúsculas ni acentos) y retomarla. Se guarda solo en tu equipo, en `%APPDATA%\com.kiook.xplanator\history.json`, con las 500 más recientes. Como el texto puede venir de cualquier app, en **⚙ Ajustes** puedes pausarlo, borrarlo entero y editar la lista de palabras que, si aparecen en el título de la ventana de origen, impiden guardar (de serie: gestores de contraseñas, bancos y navegación privada). Cada entrada se borra con su ×.

Para explicar una palabra de la respuesta, selecciónala y pulsa el botón **Explicar «…»** que aparece al lado, o Alt+X. Se abre como un nivel más, con un rastro bajo la cabecera (TCP › handshake › SYN) para volver a cualquier nivel anterior.

## Desarrollar

```bash
npm run tauri dev
```

Cierra antes la app instalada: las dos usan el mismo atajo. En desarrollo, "Iniciar con Windows" aparece desactivado; actívalo desde la app instalada.

## Cómo funciona

1. `src-tauri/src/lib.rs` registra el atajo global. Al pulsarlo, espera a que sueltes las teclas, simula Ctrl+C, lee el portapapeles y deja tu texto anterior como estaba. Con Win+Shift+Z abre la herramienta de recortes (`ms-screenclip:`), espera la imagen en el portapapeles y la lee con `Windows.Media.Ocr`.
2. Envía al recuadro el texto y el título de la ventana de origen (pista para elegir el significado correcto).
3. `src/App.vue` manda la pregunta a Rust con el prompt pedagógico y guarda el historial para las repreguntas, como texto, así que puedes cambiar de modelo a mitad de conversación.
4. Rust habla con el modelo elegido: lanza `claude -p` o `codex exec` sin servidores MCP y sin guardar la sesión, o llama a Ollama en `http://127.0.0.1:11434`. Claude va sin herramientas, sin tu configuración ni tu `CLAUDE.md`; Codex, sin shell, sin conectores, sin tu configuración ni tus reglas y en modo solo lectura (el texto seleccionado puede traer instrucciones de terceros). A ninguno le llegan claves de API del entorno, para que siempre use tu suscripción. A las CLI les reenvía la conversación entera en cada repregunta.

## Límites conocidos

- Si la ruta de la carpeta del proyecto es muy larga (más de ~120 caracteres), el enlazador de Windows falla con `LNK1104`. Usa una carpeta corta o define `CARGO_TARGET_DIR`.
- Mientras Xplanator está abierta, Alt+X deja de llegar a las demás apps; por ejemplo, en Word y Outlook ya no convierte un código Unicode en su carácter.
- El OCR usa los idiomas de reconocimiento que Windows instala con tus idiomas de pantalla. Confunde a veces letras parecidas (l e I) y puede perder símbolos sueltos, como un `=` en código; el modelo suele entenderlo igual. Si no recortas nada en un minuto, el atajo se da por cancelado.
- En una terminal sin texto seleccionado, Ctrl+C interrumpe el proceso en marcha. No uses el atajo ahí sin seleccionar algo.
- VS Code copia la línea entera si no hay selección.
- Solo se restaura texto plano. Si en el portapapeles tenías otra cosa (ficheros, una imagen) y usas el atajo con texto seleccionado, se pierde; y el texto con formato vuelve sin formato.
- Si la app de origen tarda más de medio segundo en copiar (por ejemplo, un escritorio remoto), el recuadro sale vacío y la copia sustituye a lo que tenías en el portapapeles.
- Codex carga siempre tu `AGENTS.md` global (`~/.codex/AGENTS.md`): no tiene una opción para desactivarlo sin tocar tu configuración.
- Cancelar con un modelo de Ollama que aún se está cargando en memoria, o procesando un texto largo, no corta hasta que llega el primer trozo de respuesta; mientras, la siguiente pregunta a Ollama espera.
- Solo funciona en Windows.
