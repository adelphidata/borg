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
