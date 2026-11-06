<script setup lang="ts">
import Anthropic from "@anthropic-ai/sdk";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";

// Fast model on purpose: you are waiting with the popup open, so latency matters most.
const MODEL = "claude-haiku-5-5";
const EFFORT = "low";
const REFUSAL = "El modelo ha preferido no responder a esto. Prueba a reformularlo.";

const SYSTEM_PROMPT = `Eres un profesor paciente que explica cualquier término, frase o concepto que le llegue a una persona curiosa que no es experta en ese tema.

Recibirás el texto que el usuario ha seleccionado en su pantalla y, a veces, el título de la ventana donde lo seleccionó. Usa ese título solo como pista para elegir el significado correcto. Si el término es ambiguo, di qué significado has elegido.

En la primera explicación, sigue esta estructura, en menos de 150 palabras:
1. Qué es, en una frase.
2. Una analogía o un ejemplo cotidiano.
3. Cómo funciona o por qué importa (2-4 frases).
4. Una confusión frecuente.

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

const SUGGESTION_LINE = /^\s*>>?\s*/;

const term = ref("");
const chat = ref<ChatEntry[]>([]);
const input = ref("");
const error = ref("");
const streaming = ref(false);
// With ANTHROPIC_API_KEY we pay per use through the API; without it, `claude -p` uses the subscription.
const backend = ref<"api" | "subscription">("subscription");
const body = ref<HTMLElement>();
const inputEl = ref<HTMLInputElement>();

/** One answer in flight; `cancel` aborts the API stream or kills the `claude -p` process. */
interface Request {
  id: number;
  aborted: boolean;
  cancel: () => void;
}

let client: Anthropic | null = null;
// Full conversation, sent on every follow-up. In API mode it keeps the thinking blocks unchanged.
let history: Anthropic.MessageParam[] = [];
let current: Request | null = null;
// Ids must only grow, even across hot reloads: Rust treats cancelling an id as cancelling every older one.
let nextRequestId = Date.now();

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

function describe(e: unknown): string {
  if (e instanceof Anthropic.AuthenticationError) return "La API key no es válida.";
  if (e instanceof Anthropic.RateLimitError) return "Demasiadas peticiones seguidas. Espera un momento.";
  if (e instanceof Anthropic.APIConnectionError) return "No hay conexión con la API de Claude.";
  if (e instanceof Anthropic.APIError) {
    // Errors that arrive mid-stream have no HTTP status and carry the raw JSON body in `message`.
    if (e.type === "overloaded_error") return "Claude está saturado. Prueba de nuevo en unos segundos.";
    return `Error de la API${e.status ? ` (${e.status})` : ""}: ${e.apiMessage ?? e.message}`;
  }
  return e instanceof Error ? e.message : String(e);
}

async function viaApi(
  api: Anthropic,
  messages: Anthropic.MessageParam[],
  onText: (delta: string) => void,
  request: Request,
): Promise<Anthropic.MessageParam["content"]> {
  const stream = api.messages.stream({
    model: MODEL,
    max_tokens: 8000,
    system: SYSTEM_PROMPT,
    output_config: { effort: EFFORT },
    messages,
  });
  request.cancel = () => stream.abort();
  stream.on("text", onText);
  const message = await stream.finalMessage();
  if (message.stop_reason === "refusal") throw new Error(REFUSAL);
  return message.content;
}

/** `claude -p` keeps no session here, so each call carries the conversation so far as text. */
function transcript(messages: Anthropic.MessageParam[]): string {
  // In subscription mode every message content is a plain string.
  if (messages.length === 1) return messages[0].content as string;
  const turns = messages.map((message) => {
    const tag = message.role === "user" ? "usuario" : "profesor";
    return `<${tag}>\n${message.content as string}\n</${tag}>`;
  });
  return `Conversación hasta ahora:\n\n${turns.join("\n\n")}\n\nResponde al último mensaje del usuario.`;
}

async function viaSubscription(
  messages: Anthropic.MessageParam[],
  onText: (delta: string) => void,
  request: Request,
): Promise<string> {
  let done = false;
  const onTextChannel = new Channel<string>();
  onTextChannel.onmessage = (delta) => {
    if (!done) onText(delta);
  };
  request.cancel = () => void invoke("cancel_claude_code", { requestId: request.id });
  const reply = await invoke<string>("ask_claude_code", {
    requestId: request.id,
    model: MODEL,
    effort: EFFORT,
    system: SYSTEM_PROMPT,
    prompt: transcript(messages),
    onText: onTextChannel,
  });
  done = true; // Ignore late chunks: the caller shows `reply`, the clean final text.
  return reply;
}

function cancelCurrent() {
  if (!current) return;
  current.aborted = true;
  current.cancel();
}

async function ask(content: string, shown?: string) {
  if (streaming.value) return;
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
    const reply = client
      ? await viaApi(client, [...conversation], onText, request)
      : await viaSubscription([...conversation], onText, request);
    if (request.aborted) return;
    // Claude Code may retry a failed stream and repeat or drop chunks; its final text is the clean one.
    if (typeof reply === "string") answer.text = reply;
    conversation.push({ role: "assistant", content: reply });
  } catch (e) {
    if (request.aborted) return; // A new selection or Esc cancelled it.
    conversation.pop(); // Drop the unanswered question so the next request stays valid.
    error.value = describe(e);
  } finally {
    if (current === request) {
      current = null;
      streaming.value = false;
    }
  }
}

function start(selection: Selection) {
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

function submit() {
  const text = input.value.trim();
  if (!text) return;
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
  const apiKey = await invoke<string | null>("api_key");
  if (apiKey) {
    // Safe here: the key never leaves this machine except to the Anthropic API.
    client = new Anthropic({ apiKey, dangerouslyAllowBrowser: true });
    backend.value = "api";
  }
  unlistenExplain = await listen<Selection>("explain", (event) => start(event.payload));
});

// In dev, hot reload remounts the component; without this, old copies keep listening and calling the API.
onUnmounted(() => {
  cancelCurrent();
  window.removeEventListener("keydown", onKeydown);
  unlistenExplain?.();
});
</script>

<template>
  <main class="popup">
    <header data-tauri-drag-region>
      <span class="term" data-tauri-drag-region :title="term">{{ term || "Explicador" }}</span>
      <span class="backend" data-tauri-drag-region>{{ backend === "api" ? "API" : "Suscripción" }}</span>
      <button class="close" title="Cerrar (Esc)" @click="close">×</button>
    </header>

    <section ref="body" class="body">
      <p v-if="!term" class="notice">
        Selecciona un texto en cualquier app y pulsa Win+Alt+E, o escribe abajo qué quieres que te explique.
      </p>

      <template v-for="(entry, index) in chat" :key="index">
        <p v-if="entry.role === 'user'" class="question">{{ entry.text }}</p>
        <p v-else-if="answerText(entry.text)" class="answer">{{ answerText(entry.text) }}</p>
        <p v-else-if="streaming && index === chat.length - 1" class="notice">Pensando…</p>
      </template>

      <p v-if="error" class="error">{{ error }}</p>

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

.backend {
  font-size: 12px;
  color: var(--muted);
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
