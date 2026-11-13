import { useSettings } from "../../lib/settings";
import { readyProvider, type VoiceProvider } from "../../lib/voice";
import type { Chain } from "../settings/fields";
import { withPrimaryProvider } from "../settings/voiceChain";
import { MEETINGS_ENABLED_KEY } from "./meetingsEnabled";

/**
 * Turns Meetings on or off. Turning on with no transcriber yet picks one a key
 * is already saved for — the same pick Voice input makes — so "on" works
 * without a second step whenever it can.
 */
export function useTurnOnMeetings(chain: Chain, providers: VoiceProvider[] | undefined) {
  const { save } = useSettings();
  return async (on: boolean): Promise<boolean> => {
    if (on && !chain.row && providers) {
      const pick = readyProvider(providers);
      if (pick) await chain.writeChain((json) => withPrimaryProvider(json, pick.id));
    }
    return save(MEETINGS_ENABLED_KEY, String(on));
  };
}
