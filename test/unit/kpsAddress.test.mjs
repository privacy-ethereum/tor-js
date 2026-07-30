// Unit tests for the vendored KPS address parser (src/kpsAddress.ts).
//
// This parser is a copy of @kpstreams/core's, kept so tor-js carries no runtime
// dependency on that package. A copy can drift, and a mis-parsed address means
// dialing the wrong host, so the accepted grammar is pinned here.
//
//   npm run test:unit

import { test, before, describe } from 'node:test'
import assert from 'node:assert/strict'
import { bundleTs } from './bundle.mjs'

let parseAddress, formatAddress

before(async () => {
  ;({ parseAddress, formatAddress } = await bundleTs('src/kpsAddress.ts', 'kpsAddress'))
})

const CERT = 'uEiBHwUMNRTetrbqScahm81Di57Xv2OphNrx-CurJGOq3ww'

describe('parseAddress', () => {
  test('parses an IPv4 address', () => {
    assert.deepEqual(parseAddress(`170.64.236.147:12298:${CERT}`), {
      ip: '170.64.236.147',
      port: 12298,
      certhash: CERT,
    })
  })

  test('parses a bracketed IPv6 address', () => {
    // The literal contains colons, which is why the brackets exist.
    assert.deepEqual(parseAddress(`[2606:4700::1111]:12298:${CERT}`), {
      ip: '2606:4700::1111',
      port: 12298,
      certhash: CERT,
    })
    assert.deepEqual(parseAddress(`[::1]:1:x`), { ip: '::1', port: 1, certhash: 'x' })
  })

  test('a hostname is accepted as the host (no IP validation here)', () => {
    // The parser splits; resolving is the transport's job.
    assert.equal(parseAddress(`gateway.example:12298:${CERT}`).ip, 'gateway.example')
  })

  test('the certhash keeps everything after the port', () => {
    // Base64url can contain '-' and '_', and the certhash is never colon-split.
    assert.equal(parseAddress('1.2.3.4:1:a-b_c').certhash, 'a-b_c')
    assert.equal(parseAddress('1.2.3.4:1:a:b:c').certhash, 'a:b:c')
  })

  describe('port', () => {
    test('accepts the full valid range', () => {
      assert.equal(parseAddress('1.2.3.4:1:h').port, 1)
      assert.equal(parseAddress('1.2.3.4:65535:h').port, 65535)
      assert.equal(parseAddress('1.2.3.4:00443:h').port, 443, 'leading zeros are digits')
    })

    test('rejects out-of-range ports', () => {
      for (const port of ['0', '65536', '99999', '4294967296']) {
        assert.throws(
          () => parseAddress(`1.2.3.4:${port}:h`),
          /port out of range/,
          `port ${port}`,
        )
      }
    })

    // The digits-only guard is what stops Number() coercions from being
    // accepted: Number('0x1bb') is 443 and Number(' 443 ') is 443, either of
    // which would silently dial a port the address never spelled out.
    test('rejects anything that is not plain digits', () => {
      for (const port of [
        '0x1bb',
        '1e3',
        ' 443',
        '443 ',
        '+443',
        '-443',
        '443.0',
        '4_43',
        'Infinity',
        'NaN',
        '',
        '\t443',
      ]) {
        assert.throws(() => parseAddress(`1.2.3.4:${port}:h`), /malformed/, `port ${JSON.stringify(port)}`)
      }
    })
  })

  test('rejects a missing certhash or port', () => {
    for (const s of ['1.2.3.4', '1.2.3.4:12298', '1.2.3.4:12298:', ':12298:h', '1.2.3.4::h']) {
      assert.throws(() => parseAddress(s), /malformed|out of range/, JSON.stringify(s))
    }
  })

  test('rejects an empty or malformed input', () => {
    for (const s of ['', ':', '::', 'nonsense', 'a:b:c']) {
      assert.throws(() => parseAddress(s), /malformed/, JSON.stringify(s))
    }
  })

  test('rejects malformed IPv6 bracketing', () => {
    for (const s of [
      `[2606:4700::1111:12298:${CERT}`, // no closing bracket
      `[2606:4700::1111]12298:${CERT}`, // no colon after the bracket
      `[]:12298:${CERT}`, // empty host
      `[::1]:${CERT}`, // no port
    ]) {
      assert.throws(() => parseAddress(s), /malformed/, JSON.stringify(s))
    }
  })

  test('the error names the expected shape and echoes the input', () => {
    // Operators paste these by hand; the message has to be actionable.
    assert.throws(() => parseAddress('bogus'), (e) => {
      assert.match(e.message, /<ip>:<port>:<certhash>/)
      assert.match(e.message, /\[ipv6\]/)
      assert.match(e.message, /bogus/)
      return true
    })
  })
})

describe('formatAddress', () => {
  test('round-trips IPv4 and IPv6', () => {
    for (const s of [
      `170.64.236.147:12298:${CERT}`,
      `[2606:4700::1111]:12298:${CERT}`,
      '[::1]:1:h',
      'gateway.example:443:h',
    ]) {
      assert.equal(formatAddress(parseAddress(s)), s, s)
    }
  })

  test('brackets any host containing a colon', () => {
    assert.equal(formatAddress({ ip: '::1', port: 1, certhash: 'h' }), '[::1]:1:h')
    assert.equal(formatAddress({ ip: '1.2.3.4', port: 1, certhash: 'h' }), '1.2.3.4:1:h')
  })

  test('leading zeros normalise away on the round trip', () => {
    assert.equal(formatAddress(parseAddress('1.2.3.4:00443:h')), '1.2.3.4:443:h')
  })
})
