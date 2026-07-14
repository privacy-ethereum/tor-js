// Drives the real website (built by global-setup, served statically) against
// the spawned gateway: the index page's status check dials the gateway over
// KPS/WebRTC through the ported torJsGateway.js and renders /metadata.json.
import { test, expect } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const { gatewayAddress, baseUrl } = JSON.parse(
  readFileSync(join(here, '.run-state.json'), 'utf8')
)

test('website index dials the gateway and shows its metadata', async ({ page }) => {
  page.on('pageerror', err => console.error('[page error]', err))
  page.on('console', msg => {
    if (msg.type() === 'error') console.error('[page console]', msg.text())
  })

  await page.goto(`${baseUrl}/website/index.html`)
  await page.fill('#gw-address', gatewayAddress)
  await page.click('.gw-form button')

  await page.waitForSelector('.status-dot.live', { timeout: 20000 })
  await expect(page.locator('#status-text')).toContainText('tor-js-gateway')
  await expect(page.locator('#relay-count-text')).toContainText('connect')
  // The configured address is substituted into the code examples.
  await expect(page.locator('#code-socket')).toContainText(gatewayAddress)
})
