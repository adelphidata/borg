/**
 * Checks the node transport against a running borgd.
 *
 *   BORG_HOME=~/.borg bun packages/client/scripts/smoke.mjs
 *
 * This is the exact path apps/desktop's Electron main process takes: native
 * gRPC over the unix socket. Not part of CI — it needs a live daemon — but it
 * is the fastest way to tell a client bug from a daemon bug.
 */
import { createBorgClient } from "@borg/client";
import { createNodeTransport, defaultSocketPath } from "@borg/client/node";

const socketPath = process.env.BORG_SOCK ?? defaultSocketPath();
const transport = createNodeTransport({ socketPath });
const borg = createBorgClient(transport);

try {
  console.log("socket   ", socketPath);
  console.log("vitals   ", await borg.status.getVitals({}));
  console.log("daemon   ", await borg.admin.getDaemonStatus({}));
} catch (error) {
  console.error(`FAILED (grpc code ${error.code}): ${error.message}`);
  process.exitCode = 1;
} finally {
  transport.close();
}
