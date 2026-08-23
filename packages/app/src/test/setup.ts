import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";
import { installMatchMedia } from "@borg/ui/test/matchMedia";

/**
 * Test environment shims.
 *
 * jsdom implements no layout, so it ships no `scrollIntoView` at all. Any
 * component that keeps a highlighted row in view calls it during a passive
 * effect, where a missing method is a thrown TypeError rather than a no-op.
 * Stubbing it here keeps that out of the components, which should not carry
 * guards for a gap only the test environment has.
 */
Element.prototype.scrollIntoView = () => {};

/**
 * Unmount between tests. Vitest runs without `globals`, so testing-library never
 * registers its own hook: without this, every render piles up in one document
 * and a query meant for this test also finds the previous test's copy.
 */
afterEach(cleanup);

/**
 * jsdom has no `matchMedia`, so every page that reads the mobile breakpoint
 * through `@borg/ui` would throw before its own logic ran. The default answers
 * "no" to every query — desktop width; a test that cares installs its own.
 */
installMatchMedia(() => false);
