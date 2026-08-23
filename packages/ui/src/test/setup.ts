import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";
import { installMatchMedia } from "./matchMedia";

/**
 * Test environment shims.
 *
 * jsdom has no `matchMedia`, so every component that reads the mobile
 * breakpoint would throw before its own logic ran. The default answers "no" to
 * every query — desktop width, light-scheme OS — and any test that cares
 * installs its own with `installMatchMedia`.
 */
installMatchMedia(() => false);

/**
 * jsdom implements no layout, so it ships no `scrollIntoView`. Any list that
 * keeps its highlighted row in view calls it from a passive effect, where a
 * missing method throws rather than doing nothing.
 */
Element.prototype.scrollIntoView = () => {};

/**
 * Unmount between tests. Without `globals`, testing-library never registers its
 * own hook, so every render would pile up in the same document and a
 * `getByRole` meant for one component would find the previous test's copy too.
 */
afterEach(cleanup);
