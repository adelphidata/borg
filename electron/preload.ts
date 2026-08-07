// Sole renderer<->main surface (contextIsolation); keep this API minimal and audited.
import { contextBridge } from "electron";

contextBridge.exposeInMainWorld("borg", {
  platform: process.platform,
});
