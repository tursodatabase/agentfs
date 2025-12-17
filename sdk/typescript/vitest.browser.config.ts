import { defineConfig } from 'vitest/config';
import { playwright } from "@vitest/browser-playwright"

export default defineConfig({
  server: {
    headers: {
      "Cross-Origin-Embedder-Policy": "require-corp",
      "Cross-Origin-Opener-Policy": "same-origin"
    },
  },
  test: {
    include: ['tests_browser/**/*.test.ts'],
    browser: {
      enabled: true,
      provider: playwright(),
      instances: [
        { browser: 'chromium' },
        { browser: 'firefox' }
      ],
    },
  },
});