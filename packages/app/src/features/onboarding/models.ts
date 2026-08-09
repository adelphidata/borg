/**
 * A short, opinionated model list per provider — enough to finish setup
 * without leaving the app.
 *
 * Deliberately not the full catalog: onboarding needs one good default and a
 * couple of alternatives, and a scrolling list of forty ids is how a first run
 * turns into a research task. Everything else is reachable from Settings,
 * where the model field takes any id the provider accepts.
 */
export interface ModelChoice {
  id: string;
  label: string;
  blurb: string;
}

export const MODELS_BY_PROVIDER: Record<string, ModelChoice[]> = {
  anthropic: [
    { id: "claude-sonnet-5", label: "Sonnet 5", blurb: "The balanced default." },
    { id: "claude-opus-5", label: "Opus 5", blurb: "Deepest reasoning, slower." },
    { id: "claude-haiku-4-5-20251001", label: "Haiku 4.5", blurb: "Fast and inexpensive." },
  ],
  openai: [
    { id: "gpt-5", label: "GPT-5", blurb: "The balanced default." },
    { id: "gpt-5-mini", label: "GPT-5 mini", blurb: "Faster and cheaper." },
  ],
  gemini: [
    { id: "gemini-2.5-pro", label: "Gemini 2.5 Pro", blurb: "The balanced default." },
    { id: "gemini-2.5-flash", label: "Gemini 2.5 Flash", blurb: "Fast and inexpensive." },
  ],
  deepseek: [
    { id: "deepseek-chat", label: "DeepSeek Chat", blurb: "General purpose." },
    { id: "deepseek-reasoner", label: "DeepSeek Reasoner", blurb: "Thinks before answering." },
  ],
  groq: [
    { id: "llama-3.3-70b-versatile", label: "Llama 3.3 70B", blurb: "Open weights, very fast." },
  ],
  xai: [{ id: "grok-4", label: "Grok 4", blurb: "xAI's current model." }],
  ollama: [
    { id: "llama3.2", label: "Llama 3.2", blurb: "Runs here. Nothing leaves the machine." },
    { id: "qwen2.5-coder", label: "Qwen 2.5 Coder", blurb: "Local, tuned for code." },
  ],
};

/** Models to offer for `provider`. Empty when we have no opinion. */
export function modelsFor(provider: string): ModelChoice[] {
  return MODELS_BY_PROVIDER[provider] ?? [];
}

/** The one to preselect — the first is always the recommended default. */
export function defaultModelFor(provider: string): string {
  return modelsFor(provider)[0]?.id ?? "";
}

/** Ollama runs locally, so it is the one provider that needs no API key. */
export function needsApiKey(provider: string): boolean {
  return provider !== "ollama";
}
