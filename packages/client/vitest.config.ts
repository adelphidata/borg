import { defineConfig } from "vitest/config";

/**
 * The transport seam is the whole point of this package, and it is exactly what
 * makes it testable: `node.ts` runs against a real nice-grpc server on a unix
 * socket, `web.ts` against a fake channel, and everything else is pure.
 */
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
    coverage: {
      provider: "v8",
      include: ["src/**"],
      exclude: [
        // Generated from crates/protocols/proto by `just gen`; never tested or mutated.
        "src/gen/**",
        // Barrel of re-exports — no behaviour, only wiring.
        "src/index.ts",
        "src/**/*.test.ts",
      ],
      thresholds: { lines: 90, statements: 90, functions: 90, branches: 85 },
    },
  },
});
