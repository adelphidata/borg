/**
 * Top-level specification for the browser surface (`apps/web`).
 *
 * Only properties and action generators may be exported from here.
 */
import { always } from "@antithesishq/bombadil";
import { page } from "./state.ts";

export {
  noConsoleErrors,
  noHttpErrorCodes,
  noUncaughtExceptions,
  noUnhandledPromiseRejections,
} from "@antithesishq/bombadil/browser/defaults/properties";
export * from "./properties.ts";
export { borgActions } from "./actions.ts";

/**
 * The run starts signed in and never clicks "Sign in again", so the token
 * gate must not come back: it would mean the stored token was dropped.
 */
export const tokenGateStaysClosed = always(() => !page.current.tokenGate);
