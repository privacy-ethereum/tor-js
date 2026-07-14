// Browser interop smoke test: a real Chromium page dials the gateway with
// @kpstreams/webrtc-client and fetches /metadata.json over KPS-HTTP/1,
// proving the WebRTC leg of the listener end-to-end.
import { test, expect } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const { gatewayAddress, baseUrl } = JSON.parse(
  readFileSync(join(here, '.run-state.json'), 'utf8')
)

test('browser webrtc-client fetches /metadata.json', async ({ page }) => {
  page.on('pageerror', err => console.error('[page error]', err))
  page.on('console', msg => {
    if (msg.type() === 'error') console.error('[page console]', msg.text())
  })

  await page.goto(baseUrl)
  const { status, meta } = await page.evaluate(
    addr => window.fetchMetadata(addr),
    gatewayAddress
  )
  expect(status).toBe(200)
  expect(meta.protocol).toBe('kps-http/1')
  expect(meta.software).toBe('tor-js-gateway')
  expect(meta.capabilities).toContain('connect')
  expect(meta.addresses).toContain(gatewayAddress)
  await expect(page.locator('#status')).toHaveText('done')
})
