import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

/** Unmount between tests; Vitest runs without globals, so RTL registers no hook. */
afterEach(cleanup);
