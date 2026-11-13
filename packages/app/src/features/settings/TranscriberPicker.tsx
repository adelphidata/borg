import type { VoiceProvider } from "../../lib/voice";
import { KeyField, ProviderSelect, Status, TextSetting, type Chain } from "./fields";
import { withPrimaryProvider } from "./voiceChain";

/**
 * Who turns speech into text: the provider, its key, and — for a command
 * provider — the command, over the first row of `audio.models`.
 *
 * Voice input and Meetings share one transcriber, so both pages render this.
 * `providers` is the daemon's list (`Voice.GetVoiceCatalog`); nothing here
 * names a provider. The caller owns `chain` so every write on its page goes
 * through one queue.
 */
export function TranscriberPicker({
  chain,
  providers,
  ready,
  refetch,
}: {
  chain: Chain;
  providers: VoiceProvider[];
  /** The catalog's verdict on the chain as saved. */
  ready: boolean;
  /** Re-reads the catalog after a key is saved. */
  refetch: () => void;
}) {
  const { row, writeChain, setField } = chain;
  const provider = providers.find((p) => p.id === row?.provider);
  return (
    <>
      <ProviderSelect
        label="Provider"
        providers={providers}
        value={row?.provider ?? ""}
        onPick={(id) => void writeChain((json) => withPrimaryProvider(json, id))}
      />
      {provider && <KeyField key={provider.id} provider={provider} onSaved={refetch} />}
      {provider?.requiresCommand && (
        <TextSetting
          label="Command"
          hint="{audio_file} is the recording; print the text."
          value={row?.command ?? ""}
          onCommit={(v) => setField("command", v)}
        />
      )}
      <Status
        ok={ready}
        okText="Ready"
        problem={
          provider
            ? "Not ready yet: add the missing key or command above."
            : "Choose a provider to finish."
        }
      />
    </>
  );
}
