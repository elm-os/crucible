import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    // Every scenario starts real servers, child processes and a loopback collector.
    testTimeout: 30_000,
    hookTimeout: 30_000,
  },
});
