import { defineConfig } from '@playwright/test'

export default defineConfig({
  testDir: '.',
  testMatch: /.*\.spec\.js$/,
  globalSetup: './global-setup.js',
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  reporter: 'list',
  use: {
    headless: true,
    trace: 'retain-on-failure'
  }
})
