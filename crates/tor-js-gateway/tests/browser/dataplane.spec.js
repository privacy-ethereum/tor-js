// Browser data-path tests: a real Chromium page exercises the bootstrap
// download and CONNECT tunnels over WebRTC, not just capability discovery.
//
// WebRTC is what every real browser user gets, and it is the transport where
// framing bugs have actually bitten (a reused SCTP stream id routed to a stale
// channel, kps#4). The metadata smoke test opens one stream and moves a few
// hundred bytes, which would not have caught either.
import { test, expect } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const state = JSON.parse(readFileSync(join(here, '.run-state.json'), 'utf8'))
const { gatewayAddress, baseUrl, echoTarget, bootstrapLen, bootstrapSha256 } = state

function watchPage(page) {
  page.on('pageerror', err => console.error('[page error]', err))
  page.on('console', msg => {
    if (msg.type() === 'error') console.error('[page console]', msg.text())
  })
}

test('bootstrap archive downloads byte-exact over WebRTC', async ({ page }) => {
  watchPage(page)
  await page.goto(baseUrl)

  const res = await page.evaluate(addr => window.downloadBootstrap(addr), gatewayAddress)
  expect(res.status).toBe(200)
  // 256 KB spans many data-channel messages; the hash is what proves the
  // reassembly is exact rather than merely the right length.
  expect(res.length).toBe(bootstrapLen)
  expect(res.sha256).toBe(bootstrapSha256)
  await expect(page.locator('#status')).toHaveText('done')
})

test('a CONNECT tunnel round-trips bytes and maps FIN to EOF', async ({ page }) => {
  watchPage(page)
  await page.goto(baseUrl)

  const res = await page.evaluate(
    ([addr, target]) => window.tunnelEcho(addr, target, 4096),
    [gatewayAddress, echoTarget]
  )
  expect(res.status).toBe(200)
  expect(res.length).toBe(4096)
  expect(res.matches).toBe(true)
  // §4 lifecycle: the client's FIN reaches the target, whose own FIN comes back
  // as EOF on the stream.
  expect(res.sawEof).toBe(true)
})

test('a tunnel carries a payload larger than one data-channel message', async ({ page }) => {
  watchPage(page)
  await page.goto(baseUrl)

  const res = await page.evaluate(
    ([addr, target]) => window.tunnelEcho(addr, target, 256 * 1024),
    [gatewayAddress, echoTarget]
  )
  expect(res.status).toBe(200)
  expect(res.length).toBe(256 * 1024)
  expect(res.matches).toBe(true)
})

test('a tunnel to a non-relay target is refused', async ({ page }) => {
  watchPage(page)
  await page.goto(baseUrl)

  const res = await page.evaluate(
    addr => window.tunnelEcho(addr, '203.0.113.7:9001', 16),
    gatewayAddress
  )
  expect(res.status).toBe(403)
})

test('many streams on one WebRTC connection all work', async ({ page }) => {
  watchPage(page)
  await page.goto(baseUrl)

  const results = await page.evaluate(
    ([addr, target]) => window.multiStream(addr, target),
    [gatewayAddress, echoTarget]
  )

  // Five streams in sequence on a single connection: the kps#4 regression hung
  // reads after roughly the second.
  expect(results.map(r => r.what)).toEqual([
    'metadata',
    'bootstrap',
    'tunnel1',
    'tunnel2',
    'metadata-again',
  ])
  for (const r of results) expect(r.status).toBe(200)
  expect(results.find(r => r.what === 'bootstrap').length).toBe(bootstrapLen)
  expect(results.find(r => r.what === 'tunnel1').text).toBe('tunnel 1')
  expect(results.find(r => r.what === 'tunnel2').text).toBe('tunnel 2')
})
