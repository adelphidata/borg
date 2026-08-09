import { createContext, useContext, type ReactNode } from "react";
import type { BorgTransport } from "@borg/client";

/**
 * Everything that differs between the Electron window and the browser tab.
 * Feature code reads this instead of sniffing `window`, so pages stay identical
 * across surfaces.
 */
export interface Platform {
  kind: "desktop" | "web";
  /** How this surface reaches borgd: IPC→UDS on desktop, gRPC-Web on the browser. */
  transport: BorgTransport;
  /** Reserve a draggable strip for a frameless window. Desktop only. */
  titleBarDrag: boolean;
}

const PlatformContext = createContext<Platform | null>(null);

export function PlatformProvider({
  platform,
  children,
}: {
  platform: Platform;
  children: ReactNode;
}) {
  return <PlatformContext.Provider value={platform}>{children}</PlatformContext.Provider>;
}

export function usePlatform(): Platform {
  const ctx = useContext(PlatformContext);
  if (!ctx) throw new Error("usePlatform must be used inside <PlatformProvider>");
  return ctx;
}

/** The daemon transport for the current surface. */
export function useTransport(): BorgTransport {
  return usePlatform().transport;
}
