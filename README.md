# Explicador

Selecciona un texto en cualquier app de Windows, pulsa **Win+Alt+E** y aparece un recuadro con una explicación breve y didáctica del término. Puedes repreguntar escribiendo o pulsando una de las preguntas sugeridas.

Hecho con Tauri v2 (Rust) + Vue 3 + TypeScript y la API de Claude (modelo `claude-haiku-5-5`).

## Requisitos

- Node.js y Rust (`rustup`, toolchain `stable-msvc`).
- Build Tools de Visual Studio con C++ y el SDK de Windows.
- Una API key de Anthropic en la variable de entorno de usuario `ANTHROPIC_API_KEY`.
  La forma más segura de crearla es la ventana de Windows "Editar las variables de entorno de esta cuenta" (así la clave no queda en el historial de la terminal). Después, abre una terminal nueva.

## Arrancar

```bash
npm install
npm run tauri dev
```

La app arranca oculta, con un icono en la bandeja del sistema. Para salir: clic derecho en el icono → **Salir**.

En el recuadro: **Esc** lo oculta y la cabecera sirve para arrastrarlo.

## Cómo funciona

1. `src-tauri/src/lib.rs` registra el atajo global. Al pulsarlo, espera a que sueltes las teclas, simula Ctrl+C, lee el portapapeles y deja tu texto anterior como estaba.
2. Envía al recuadro el texto y el título de la ventana de origen (pista para elegir el significado correcto).
3. `src/App.vue` llama a Claude en streaming con el prompt pedagógico y guarda el historial para las repreguntas.

## Límites conocidos

- Si la ruta de la carpeta del proyecto es muy larga (más de ~120 caracteres), el enlazador de Windows falla con `LNK1104`. Usa una carpeta corta o define `CARGO_TARGET_DIR`.
- En una terminal sin texto seleccionado, Ctrl+C interrumpe el proceso en marcha. No uses el atajo ahí sin seleccionar algo.
- VS Code copia la línea entera si no hay selección.
- Solo se restaura texto plano. Si en el portapapeles tenías otra cosa (ficheros, una imagen) y usas el atajo con texto seleccionado, se pierde; y el texto con formato vuelve sin formato.
- Si la app de origen tarda más de medio segundo en copiar (por ejemplo, un escritorio remoto), el recuadro sale vacío y la copia sustituye a lo que tenías en el portapapeles.
- Solo funciona en Windows.
