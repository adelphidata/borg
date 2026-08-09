import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BorgApp, type Platform } from "@borg/app";
import "@borg/ui/styles/fonts.css";
import "@borg/ui/styles/app.css";

/**
 * Desktop shell. All product code lives in `@borg/app`; this file only says
 * *how* this surface reaches borgd and that the window needs a drag strip.
 *
 * The transport forwards to Electron's main process, which owns the real gRPC
 * channel over borgd's unix socket — the renderer never touches Node APIs.
 */
const platform: Platform = {
  kind: "desktop",
  transport: window.borg.transport,
  titleBarDrag: window.borg.platform === "darwin",
};

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BorgApp platform={platform} />
  </StrictMode>,
);
