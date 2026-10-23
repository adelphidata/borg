import { useNavigate } from "react-router-dom";
import { ChevronIcon, Pill, SECTION_LABEL, StatusDot } from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import type { DaemonSetting } from "../../lib/settings";
import { useMutation } from "../../lib/useDaemon";
import { providerLabel, providerOf } from "../onboarding/useSetupStatus";
import { SETTING_NAME, SETTING_ROW } from "./AdvancedSettings";
import { settingPath } from "./catalog";
import { SIGN_INS, keySummary, keysFailing, useProviderKeys } from "./providerKeys";
import { summarizeValue } from "./settingValue";

/**
 * The settings a provider reads beyond its key, in the order they are asked.
 * The daemon composes Azure's URL and Vertex's from these, Bedrock's region
 * and endpoint, and the address of Ollama or a custom endpoint.
 */
const PROVIDER_FIELDS: Record<string, string[]> = {
  bedrock: ["llm.bedrock_region", "llm.bedrock_endpoint_url", "llm.bedrock_use_fips"],
  ollama: ["llm.base_url"],
  "azure-openai": ["llm.azure_resource", "llm.azure_deployment", "llm.azure_api_version"],
  "vertex-ai": ["llm.vertex_project", "llm.vertex_location"],
  custom: ["llm.base_url"],
};

/** A custom endpoint is most often a keyless local server; its key is optional. */
const KEY_OPTIONAL = new Set(["custom"]);

/** Where a setting editor opened from here should lead back to. */
const BACK_TO_PROVIDER = { back: { to: "/settings/provider", label: "Provider" } };

/**
 * How a keyless provider reaches its model, in one line, and whether that
 * works on this machine right now. Undefined for a provider with a key.
 */
function runtimeStatus(
  provider: string,
  detected: admin.DetectedCredential[],
): { ok: boolean; text: string } | undefined {
  const found = (source: string) =>
    detected.find((d) => d.provider.toLowerCase() === provider && d.source === source);
  switch (provider) {
    case "claude-cli": {
      const cli = found("claude_cli");
      return cli
        ? { ok: true, text: `Signed in. Borg runs Claude Code from ${cli.detail}.` }
        : {
            ok: false,
            text: "Claude Code is not signed in on this machine. Install it, then run claude once to sign in.",
          };
    }
    case "ollama": {
      const server = found("ollama");
      return server
        ? { ok: true, text: `Running at ${server.detail}.` }
        : { ok: false, text: "Ollama is not answering. Start it, then come back." };
    }
    case "bedrock":
      return {
        ok: true,
        text: "Borg signs in with the AWS credentials on this machine: AWS_* variables or ~/.aws.",
      };
    case "vertex-ai":
      return {
        ok: true,
        text: "Borg signs in with the service-account file in GOOGLE_APPLICATION_CREDENTIALS, or with this machine's own Google Cloud account.",
      };
    default:
      return undefined;
  }
}

/** Marks a pool whose every key has stopped working; the row's count alone does not say so. */
const FAILING = "Keys not working";

function Row({
  name,
  value,
  alert,
  failing,
  onOpen,
}: {
  name: string;
  value: string;
  /** The value itself is the problem ("None saved"). */
  alert?: boolean;
  failing?: boolean;
  onOpen: () => void;
}) {
  return (
    <button type="button" className={SETTING_ROW} onClick={onOpen}>
      <span className={SETTING_NAME}>{name}</span>
      {failing && <StatusDot tone="alert" label={FAILING} />}
      <span
        className={`max-w-[220px] flex-none truncate text-[13px] leading-none ${
          alert ? "text-alert" : "text-ink2"
        }`}
      >
        {value}
      </span>
      <ChevronIcon size={13} className="flex-none text-ink3" />
    </button>
  );
}

/** Imports the sign-in a tool on this machine already holds into the provider's pool. */
function ImportSignIn({
  signIn,
  onDone,
}: {
  signIn: (typeof SIGN_INS)[number];
  onDone: () => void;
}) {
  const pair = useMutation((borg, kind: string) => borg.admin.pairCredential({ kind }), {
    onSuccess: onDone,
  });
  const found = pair.data?.entries.length;
  return (
    <div className="flex flex-col gap-2.5 py-3">
      <Pill
        square
        disabled={pair.pending}
        className="self-start"
        onClick={() => void pair.run(signIn.kind)}
      >
        {pair.pending ? "Importing…" : `Import from ${signIn.label}`}
      </Pill>
      {pair.error && (
        <div className="text-[12.5px] leading-[1.45] text-alert">{pair.error.message}</div>
      )}
      {found !== undefined && (
        <div className="text-[12.5px] leading-[1.45] text-ink2">
          {found === 0
            ? `Nothing found on this machine — sign in with ${signIn.label} first.`
            : `Imported ${found} account${found === 1 ? "" : "s"}.`}
        </div>
      )}
    </div>
  );
}

/**
 * Everything the provider in force needs, under the Provider picker: the
 * settings it reads, whether its local runtime answers, its keys or the
 * sign-in it imports. Below that, the keys saved for every other provider,
 * so a pool stays in reach after a switch.
 */
export function ProviderSetup({
  provider,
  settings,
}: {
  provider: string;
  settings: Map<string, DaemonSetting>;
}) {
  const navigate = useNavigate();
  const keys = useProviderKeys([provider]);
  const id = provider.toLowerCase();

  return (
    <DaemonState query={keys} label="your provider keys">
      {(data) => {
        const choice = providerOf(data.catalog, id);
        const label = providerLabel(data.catalog, id);
        const status = runtimeStatus(id, data.detected);
        const fields = (PROVIDER_FIELDS[id] ?? []).flatMap((key) => {
          const entry = settings.get(key);
          return entry ? [entry] : [];
        });
        const signIn = SIGN_INS.find((s) => s.provider === id);
        const mine = data.providers.find((p) => p.name === id);
        const summary = keySummary(mine);
        const keyed = choice?.needsApiKey ?? mine !== undefined;
        const optional = KEY_OPTIONAL.has(id);
        const others = data.providers.filter((p) => p.name !== id && keySummary(p));
        const openKeys = (name: string) =>
          navigate(`/settings/provider/${encodeURIComponent(name)}`);

        return (
          <>
            {id !== "" && (status || fields.length > 0 || keyed || signIn) && (
              <section>
                <div className={SECTION_LABEL}>Set up {label}</div>
                {status && (
                  <div className="flex items-start gap-2.5 py-2 text-[13px] leading-[1.55] text-pretty text-ink2">
                    <span className="mt-[7px]">
                      <StatusDot tone={status.ok ? "ok" : "alert"} />
                    </span>
                    <span className="min-w-0">{status.text}</span>
                  </div>
                )}
                {signIn && <ImportSignIn signIn={signIn} onDone={keys.refetch} />}
                {fields.map((f) => (
                  <Row
                    key={f.key}
                    name={f.label || f.key}
                    value={f.value === "" ? "Not set" : summarizeValue(f.key, f.value, f.kind)}
                    onOpen={() => navigate(settingPath(f.key), { state: BACK_TO_PROVIDER })}
                  />
                ))}
                {keyed && (
                  <Row
                    name={
                      signIn
                        ? "Sign-ins and keys"
                        : optional
                          ? "API key, if it needs one"
                          : "API key"
                    }
                    value={summary ?? (optional ? "None" : "None saved")}
                    alert={!summary && !optional}
                    failing={keysFailing(mine)}
                    onOpen={() => openKeys(id)}
                  />
                )}
              </section>
            )}
            {others.length > 0 && (
              <section>
                <div className={SECTION_LABEL}>Keys for other providers</div>
                {others.map((p) => (
                  <Row
                    key={p.name}
                    name={providerLabel(data.catalog, p.name)}
                    value={keySummary(p)!}
                    failing={keysFailing(p)}
                    onOpen={() => openKeys(p.name)}
                  />
                ))}
              </section>
            )}
          </>
        );
      }}
    </DaemonState>
  );
}
