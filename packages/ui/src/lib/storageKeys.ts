import keys from "../../storage-keys.json";

/**
 * localStorage keys shared across surfaces. Change them in
 * `packages/ui/storage-keys.json`, which is the single source: the React code
 * imports it here, and `vite.ts` reads the same file to build the pre-paint
 * theme script. Neither can drift from the other.
 *
 * `token` is the gateway client token and is used by the browser surface only.
 */
export const STORAGE_KEYS = keys;
