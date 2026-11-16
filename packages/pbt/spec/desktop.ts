/**
 * Top-level specification for the desktop surface (`apps/desktop`), driven
 * through Electron's remote debugger with `bombadil browser test-external`.
 *
 * Only properties and action generators may be exported from here.
 */
import { always } from "@antithesishq/bombadil";
import { page } from "./state.ts";

export {
  noConsoleErrors,
  noUncaughtExceptions,
  noUnhandledPromiseRejections,
} from "@antithesishq/bombadil/browser/defaults/properties";
export * from "./properties.ts";
export { borgActions } from "./actions.ts";

/** The unix socket is the desktop's auth; a token prompt here is a surface mix-up. */
export const noTokenGateOnDesktop = always(() => !page.current.tokenGate);
