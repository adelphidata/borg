import type { admin } from "@borg/client";

/**
 * A small `Admin.GetSetupStatus` answer for tests: two keyed providers (one
 * recommended and featured, one behind the fold) and one keyless one. Small
 * on purpose — a test that needs a fourth row should say so where it adds it.
 */
export const SETUP_CATALOG: admin.ProviderChoice[] = [
  {
    id: "openrouter",
    label: "OpenRouter",
    blurb: "One key, every model behind it.",
    keyUrl: "https://openrouter.ai/keys",
    needsApiKey: true,
    recommended: true,
    featured: true,
    defaultModel: "deepseek/deepseek-v4-flash",
    models: [
      { id: "deepseek/deepseek-v4-flash", label: "DeepSeek V4 Flash", note: "" },
      { id: "anthropic/claude-opus-5", label: "Claude Opus 5", note: "" },
    ],
  },
  {
    id: "anthropic",
    label: "Anthropic",
    blurb: "Claude, straight from the source.",
    keyUrl: "https://console.anthropic.com/settings/keys",
    needsApiKey: true,
    recommended: false,
    featured: true,
    defaultModel: "claude-opus-5",
    models: [
      { id: "claude-opus-5", label: "Opus 5", note: "" },
      { id: "claude-sonnet-5", label: "Sonnet 5", note: "fast" },
    ],
  },
  {
    id: "groq",
    label: "Groq",
    blurb: "Open models at very high speed.",
    keyUrl: "https://console.groq.com/keys",
    needsApiKey: true,
    recommended: false,
    featured: false,
    defaultModel: "openai/gpt-oss-120b",
    models: [{ id: "openai/gpt-oss-120b", label: "GPT-OSS 120B", note: "" }],
  },
  {
    id: "ollama",
    label: "Ollama",
    blurb: "A model running on this machine. Nothing leaves.",
    keyUrl: "",
    needsApiKey: false,
    recommended: false,
    featured: false,
    defaultModel: "llama3.3",
    models: [{ id: "llama3.3", label: "Llama 3.3 70B", note: "" }],
  },
];

export function setupStatus(over: Partial<admin.SetupStatus> = {}): admin.SetupStatus {
  return {
    configured: false,
    provider: "",
    model: "",
    providers: SETUP_CATALOG,
    detected: [],
    agentName: "",
    agentNames: ["Borgzilla", "Borg Vader"],
    ...over,
  };
}
