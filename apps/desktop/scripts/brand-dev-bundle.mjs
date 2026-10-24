/**
 * Gives the dev app the packaged app's name, version, and icon on macOS.
 *
 * Unpackaged, the app runs inside the stock Electron.app from node_modules,
 * and macOS reads the app menu title, the About panel, and the Dock label from
 * that bundle's Info.plist — so all three said "Electron". No runtime call can
 * rename the app menu, so this rewrites the bundle the way electron-builder
 * brands the packaged one. The bundle's signature does not seal its Info.plist
 * or resources, so the edit leaves it launchable. Safe to run on every start.
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";

if (process.platform === "darwin") {
  const root = path.resolve(import.meta.dirname, "..");
  const builder = readFileSync(path.join(root, "electron-builder.yml"), "utf8");
  const name = builder.match(/^productName:\s*(.+)$/m)[1].trim();
  const { version } = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));

  // `require("electron")` is the path of Electron.app/Contents/MacOS/Electron.
  const contents = path.resolve(createRequire(import.meta.url)("electron"), "../..");
  const plist = path.join(contents, "Info.plist");
  for (const [key, value] of [
    ["CFBundleName", name],
    ["CFBundleDisplayName", name],
    ["CFBundleShortVersionString", version],
    ["CFBundleVersion", version],
  ]) {
    execFileSync("plutil", ["-replace", key, "-string", value, plist]);
  }
  copyFileSync(
    path.join(root, "build", "icon.icns"),
    path.join(contents, "Resources", "electron.icns"),
  );
}
