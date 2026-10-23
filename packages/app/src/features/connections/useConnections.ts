import type { BorgClient, admin } from "@borg/client";
import { useQuery, type QueryResult } from "../../lib/useDaemon";

/**
 * The messaging channels Borg answers on, with approved senders and pending
 * pairing requests. Model providers are not connections: their keys live
 * under Settings → Provider.
 */

export interface ChannelConnection {
  kind: "channel";
  name: string;
  senders: admin.ApprovedSender[];
  pending: admin.PairingRequest[];
}

export interface Connections {
  channels: ChannelConnection[];
  /** Total pending pairing requests across all channels. */
  pendingCount: number;
}

/** Two-letter tile badge for any connection name. */
export function badgeOf(name: string): string {
  return (
    name
      .replace(/[^a-zA-Z0-9]/g, "")
      .slice(0, 2)
      .toUpperCase() || "??"
  );
}

export async function loadConnections(borg: BorgClient): Promise<Connections> {
  const [senders, pairings] = await Promise.all([
    borg.admin.listApprovedSenders({}),
    borg.admin.listPairingRequests({}),
  ]);

  const channels = new Map<string, ChannelConnection>();
  const channelOf = (name: string): ChannelConnection => {
    const existing = channels.get(name);
    if (existing) return existing;
    const fresh: ChannelConnection = { kind: "channel", name, senders: [], pending: [] };
    channels.set(name, fresh);
    return fresh;
  };
  for (const s of senders.senders) channelOf(s.channelName).senders.push(s);
  for (const r of pairings.requests) {
    if (r.status === "pending") channelOf(r.channelName).pending.push(r);
  }

  const channelList = [...channels.values()].sort((a, b) => a.name.localeCompare(b.name));
  return {
    channels: channelList,
    pendingCount: channelList.reduce((n, c) => n + c.pending.length, 0),
  };
}

export function useConnections(): QueryResult<Connections> {
  return useQuery(loadConnections);
}
