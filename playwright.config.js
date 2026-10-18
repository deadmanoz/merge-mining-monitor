const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL || "http://127.0.0.1:18080",
  },
  webServer: process.env.PLAYWRIGHT_BASE_URL
    ? undefined
    : {
        command: "SERVE_BIND_ADDR=127.0.0.1:18080 cargo run -- serve",
        url: "http://127.0.0.1:18080/health",
        timeout: 120_000,
        reuseExistingServer: !process.env.CI,
      },
});
