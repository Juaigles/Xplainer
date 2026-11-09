<script setup lang="ts">
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";

const EFFORT = "low";

const SYSTEM_PROMPT = `Eres un profesor paciente que explica cualquier texto que le llegue (un término, un párrafo, un fragmento de código o un mensaje de error) a una persona curiosa que no es experta en ese tema.

Recibirás el texto que el usuario ha seleccionado en su pantalla y, a veces, el título de la ventana donde lo seleccionó. Usa ese título solo como pista para elegir el significado correcto.

En la primera explicación, decide en silencio qué tipo de texto es y sigue la estructura que le corresponde, en menos de 150 palabras. Tu respuesta empieza por el punto 1: no nombres el tipo de texto ni la estructura.

Si es un término o un concepto:
1. Qué es, en una frase. Si el término es ambiguo, di en esa misma frase qué significado has elegido.
2. Una analogía o un ejemplo cotidiano.
3. Cómo funciona o por qué importa (2-4 frases).
4. Una confusión frecuente.

Si es una frase o un párrafo:
1. La idea principal, en una frase y con palabras sencillas.
2. Qué significan los 2 o 3 términos clave para entenderlo.
3. Qué contexto hace falta para que tenga sentido.

Si es código:
1. Qué hace, en una frase.
2. Cómo lo hace, recorriendo sus partes importantes en orden.
3. Un detalle que no es obvio o un error típico al usarlo.

Si es un mensaje de error, un aviso o un registro con fallos:
1. Qué significa, en una frase.
2. La causa más probable y, si la hay, otra posible.
3. Cómo comprobarlo, con pasos concretos.

En las preguntas de seguimiento, responde directamente a lo que se pregunta, en menos de 150 palabras y con ejemplos concretos.

Escribe en español y en texto plano: sin Markdown, sin negritas y sin encabezados.

Termina siempre con exactamente tres líneas que empiecen por ">> ", cada una con una pregunta corta que el usuario podría hacerte para profundizar.`;

/** Payload of the "explain" event that Rust emits when the shortcut is pressed. */
interface Selection {
  text: string;
  window_title: string;
}

interface ChatEntry {
  role: "user" | "assistant";
  text: string;
}

/** The conversation as every model receives it: plain text, so you can switch models midway. */
interface Message {
  role: "user" | "assistant";
  content: string;
}

/** A model in the selector: Claude and Codex use your subscriptions; Ollama models are free. */
interface ModelOption {
  id: string;
  label: string;
  provider: "claude" | "codex" | "ollama";
  model: string;
}

// Fast models on purpose: you are waiting with the popup open, so latency matters most.
const SUBSCRIPTION_MODELS: ModelOption[] = [
  { id: "claude", label: "Claude Haiku · suscripción", provider: "claude", model: "claude-haiku-5-5" },
  { id: "codex", label: "Codex GPT-5.6 Luna · suscripción", provider: "codex", model: "gpt-5.6-luna" },
];
const STORAGE_KEY = "xplanator.model";

const SUGGESTION_LINE = /^\s*>>?\s*/;

/** Follow-ups under the last answer that change its level without typing. */
const REPHRASINGS = [
  { label: "Más simple", prompt: "Explícamelo de forma más simple, como si no supiera nada del tema." },
  { label: "Más técnico", prompt: "Explícamelo de forma más técnica y precisa, con los términos correctos del área." },
  {
    label: "Otro significado",
    prompt: "¿Qué otro significado puede tener lo que seleccioné? Explícame el más probable después del que ya me has dado.",
    onlyShortTerms: true,
  },
];

const term = ref("");
const chat = ref<ChatEntry[]>([]);
const input = ref("");
const error = ref("");
const streaming = ref(false);
const ollamaModels = ref<string[]>([]);
const selectedId = ref(readStoredModel());
const body = ref<HTMLElement>();
const inputEl = ref<HTMLInputElement>();

const models = computed<ModelOption[]>(() => [
  ...SUBSCRIPTION_MODELS,
  ...ollamaModels.value.map((name) => ({
    id: `ollama:${name}`,
    label: `${name} · ${name.includes("cloud") ? "nube de Ollama" : "local"}`,
    provider: "ollama" as const,
    model: name,
  })),
]);

/** One answer in flight; `cancel` stops it in Rust. */
interface Request {
  id: number;
  aborted: boolean;
  cancel: () => void;
}

// Full conversation, sent on every follow-up.
let history: Message[] = [];
let current: Request | null = null;
// Ids must only grow, even across hot reloads: Rust treats cancelling an id as cancelling every older one.
let nextRequestId = Date.now();

function readStoredModel(): string {
  try {
    return localStorage.getItem(STORAGE_KEY) ?? "claude";
  } catch {
    return "claude";
  }
}

watch(selectedId, (id) => {
  try {
    localStorage.setItem(STORAGE_KEY, id);
  } catch {
    // Only a convenience: without storage the app starts with Claude.
  }
});

async function refreshOllamaModels() {
  ollamaModels.value = await invoke<string[]>("ollama_models");
}

const rephrasings = computed(() => {
  const last = chat.value[chat.value.length - 1];
  if (streaming.value || last?.role !== "assistant" || !answerText(last.text)) return [];
  // "Another meaning" only makes sense for a word or a short expression.
  const shortTerm = !term.value.includes("\n") && term.value.split(/\s+/).length <= 4;
  return REPHRASINGS.filter((option) => shortTerm || !option.onlyShortTerms);
});

const suggestions = computed(() => {
  const last = chat.value[chat.value.length - 1];
  if (streaming.value || last?.role !== "assistant") return [];
  return last.text
    .split("\n")
    .filter((line) => SUGGESTION_LINE.test(line))
    .map((line) => line.replace(SUGGESTION_LINE, "").trim())
    .filter(Boolean);
});

function answerText(text: string): string {
  return text
    .split("\n")
    .filter((line) => !SUGGESTION_LINE.test(line))
    .join("\n")
    .trim();
}

function firstPrompt(text: string, windowTitle: string): string {
  const context = windowTitle ? `\nVentana donde lo seleccioné: ${windowTitle}` : "";
  return `Explícame esto:\n<seleccion>\n${text}\n</seleccion>${context}`;
}

/** The CLIs keep no session here, so each call carries the conversation so far as text. */
function transcript(messages: Message[]): string {
  if (messages.length === 1) return messages[0].content;
  const turns = messages.map((message) => {
    const tag = message.role === "user" ? "usuario" : "profesor";
    return `<${tag}>\n${message.content}\n</${tag}>`;
  });
  return `Conversación hasta ahora:\n\n${turns.join("\n\n")}\n\nResponde al último mensaje del usuario.`;
}

/** Asks the chosen model through Rust; partial text arrives through `onText` while it answers. */
async function answerWith(
  option: ModelOption,
  messages: Message[],
  onText: (delta: string) => void,
  request: Request,
): Promise<string> {
  let done = false;
  const onTextChannel = new Channel<string>();
  onTextChannel.onmessage = (delta) => {
    if (!done) onText(delta);
  };
  request.cancel = () => void invoke("cancel_answer", { requestId: request.id });
  const common = { requestId: request.id, model: option.model, onText: onTextChannel };
  let reply: string;
  if (option.provider === "claude") {
    reply = await invoke<string>("ask_claude_code", {
      ...common,
      effort: EFFORT,
      system: SYSTEM_PROMPT,
      prompt: transcript(messages),
    });
  } else if (option.provider === "codex") {
    // Codex runs through `cmd`, which cannot carry these multi-line instructions as an
    // argument, so they go first in the prompt.
    reply = await invoke<string>("ask_codex", {
      ...common,
      effort: EFFORT,
      prompt: `${SYSTEM_PROMPT}\n\n---\n\n${transcript(messages)}`,
    });
  } else {
    reply = await invoke<string>("ask_ollama", { ...common, system: SYSTEM_PROMPT, messages });
  }
  done = true; // Ignore late chunks: the caller shows `reply`, the clean final text.
  return reply;
}

function cancelCurrent() {
  if (!current) return;
  current.aborted = true;
  current.cancel();
}

const UNAVAILABLE = "Ese modelo no está disponible (¿está Ollama en marcha?). Elige otro en el desplegable.";

// Bumped by start(): an ask still waiting for the model list then belongs to an old selection.
let generation = 0;

async function chosenModel(): Promise<ModelOption | undefined> {
  const find = () => models.value.find((model) => model.id === selectedId.value);
  // Ollama may have started after the app: look again before giving up.
  if (!find() && selectedId.value.startsWith("ollama:")) await refreshOllamaModels();
  return find();
}

async function ask(content: string, shown?: string) {
  if (streaming.value) return;
  const started = generation;
  const option = await chosenModel();
  if (started !== generation || streaming.value) return;
  if (!option) {
    error.value = UNAVAILABLE;
    return;
  }
  const conversation = history;
  conversation.push({ role: "user", content });
  if (shown) chat.value.push({ role: "user", text: shown });
  chat.value.push({ role: "assistant", text: "" });
  const answer = chat.value[chat.value.length - 1];
  const onText = (delta: string) => {
    answer.text += delta;
  };
  error.value = "";
  streaming.value = true;
  const request: Request = { id: nextRequestId++, aborted: false, cancel: () => {} };
  current = request;

  try {
    const reply = await answerWith(option, [...conversation], onText, request);
    if (request.aborted) return;
    // A CLI may retry a failed stream and repeat or drop chunks; the final text is the clean one.
    answer.text = reply;
    conversation.push({ role: "assistant", content: reply });
  } catch (e) {
    if (request.aborted) return; // A new selection or Esc cancelled it.
    conversation.pop(); // Drop the unanswered question so the next request stays valid.
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    if (current === request) {
      current = null;
      streaming.value = false;
    }
  }
}

function start(selection: Selection) {
  generation++;
  cancelCurrent();
  current = null;
  streaming.value = false;
  history = [];
  chat.value = [];
  error.value = "";
  input.value = "";
  term.value = selection.text.trim();
  if (term.value) {
    void ask(firstPrompt(term.value, selection.window_title));
  } else {
    void nextTick(() => inputEl.value?.focus());
  }
}

async function submit() {
  const text = input.value.trim();
  if (!text) return;
  if (!(await chosenModel())) {
    error.value = UNAVAILABLE; // Keep the typed text so it can be sent after choosing a model.
    return;
  }
  input.value = "";
  if (history.length === 0) {
    term.value = text;
    void ask(firstPrompt(text, ""));
  } else {
    void ask(text, text);
  }
}

function close() {
  cancelCurrent();
  void getCurrentWindow().hide();
}

watch(
  [chat, error],
  () => nextTick(() => body.value?.scrollTo({ top: body.value.scrollHeight })),
  { deep: true },
);

function onKeydown(event: KeyboardEvent) {
  if (event.key === "Escape") close();
}

let unlistenExplain: UnlistenFn | undefined;

onMounted(async () => {
  window.addEventListener("keydown", onKeydown);
  // Listen first: a shortcut pressed while Ollama is slow to answer must not be lost.
  unlistenExplain = await listen<Selection>("explain", (event) => start(event.payload));
  void refreshOllamaModels();
});

// In dev, hot reload remounts the component; without this, old copies keep listening and answering.
onUnmounted(() => {
  cancelCurrent();
  window.removeEventListener("keydown", onKeydown);
  unlistenExplain?.();
});
</script>

<template>
  <main class="popup">
    <header data-tauri-drag-region>
      <span class="term" data-tauri-drag-region :title="term">{{ term || "Xplanator" }}</span>
      <select v-model="selectedId" class="model" title="Modelo que explica" @focus="refreshOllamaModels">
        <option v-for="option in models" :key="option.id" :value="option.id">{{ option.label }}</option>
      </select>
      <button class="close" title="Cerrar (Esc)" @click="close">×</button>
    </header>

    <section ref="body" class="body">
      <p v-if="!term" class="notice">
        Selecciona un texto en cualquier app y pulsa Alt+X, o escribe abajo qué quieres que te explique.
      </p>

      <template v-for="(entry, index) in chat" :key="index">
        <p v-if="entry.role === 'user'" class="question">{{ entry.text }}</p>
        <p v-else-if="answerText(entry.text)" class="answer">{{ answerText(entry.text) }}</p>
        <p v-else-if="streaming && index === chat.length - 1" class="notice">Pensando…</p>
      </template>

      <p v-if="error" class="error">{{ error }}</p>

      <div v-if="rephrasings.length" class="rephrasings">
        <button v-for="option in rephrasings" :key="option.label" @click="ask(option.prompt, option.label)">
          {{ option.label }}
        </button>
      </div>

      <div v-if="suggestions.length" class="suggestions">
        <button v-for="question in suggestions" :key="question" @click="ask(question, question)">
          {{ question }}
        </button>
      </div>
    </section>

    <form @submit.prevent="submit">
      <input
        ref="inputEl"
        v-model="input"
        :placeholder="term ? 'Pregunta lo que no entiendas…' : '¿Qué quieres que te explique?'"
      />
      <button :disabled="!input.trim() || streaming">Enviar</button>
    </form>
  </main>
</template>

<style>
:root {
  --bg: #ffffff;
  --fg: #1f2328;
  --muted: #656d76;
  --border: #d0d7de;
  --accent: #6d4aff;
  --on-accent: #ffffff;
  --accent-bg: #f1edff;
  --error: #cf222e;
  font-family: "Segoe UI", system-ui, sans-serif;
  font-size: 14px;
  line-height: 1.5;
  color: var(--fg);
  background: var(--bg);
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1e1f22;
    --fg: #e6e6e6;
    --muted: #9aa0a6;
    --border: #3a3d42;
    --accent: #a993ff;
    --on-accent: #1e1f22;
    --accent-bg: #2c2645;
    --error: #ff7b72;
  }
}

html,
body,
#app {
  height: 100%;
  margin: 0;
}

.popup {
  display: flex;
  flex-direction: column;
  height: 100%;
  border: 1px solid var(--border);
  box-sizing: border-box;
}

header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 8px 8px 14px;
  border-bottom: 1px solid var(--border);
  user-select: none;
}

.term {
  flex: 1;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.model {
  max-width: 45%;
  padding: 2px 4px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--bg);
  color: var(--fg);
  font: inherit;
  font-size: 12px;
}

.close {
  border: none;
  background: none;
  color: var(--muted);
  font-size: 18px;
  cursor: pointer;
}

.body {
  flex: 1;
  overflow-y: auto;
  padding: 12px 14px;
}

.answer {
  white-space: pre-wrap;
  margin: 0 0 12px;
}

.question {
  margin: 12px 0 8px;
  padding: 6px 10px;
  border-radius: 8px;
  background: var(--accent-bg);
}

.notice {
  color: var(--muted);
}

.error {
  color: var(--error);
}

.rephrasings {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 8px;
}

.rephrasings button {
  padding: 2px 10px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: none;
  color: var(--muted);
  font: inherit;
  font-size: 12px;
  cursor: pointer;
}

.rephrasings button:hover {
  border-color: var(--accent);
  color: var(--fg);
}

.suggestions {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.suggestions button {
  text-align: left;
  padding: 6px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: none;
  color: var(--fg);
  font: inherit;
  cursor: pointer;
}

.suggestions button:hover {
  border-color: var(--accent);
}

form {
  display: flex;
  gap: 8px;
  padding: 10px;
  border-top: 1px solid var(--border);
}

input {
  flex: 1;
  padding: 6px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--bg);
  color: var(--fg);
  font: inherit;
}

form button {
  padding: 6px 12px;
  border: none;
  border-radius: 8px;
  background: var(--accent);
  color: var(--on-accent);
  font: inherit;
  cursor: pointer;
}

form button:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
