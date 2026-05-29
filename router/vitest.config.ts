import { defineConfig } from "vitest/config";

// Plain node environment: the handlers are pure (registry + injected fetch), so
// they don't need the workers runtime pool to unit-test.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
  },
});
