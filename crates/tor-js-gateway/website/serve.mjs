// Minimal static file server for local preview (no dependencies).
import { createServer } from 'node:http'
import { readFile } from 'node:fs/promises'
import { extname, join, normalize } from 'node:path'
import { dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(fileURLToPath(import.meta.url))
const port = parseInt(process.env.PORT || '8080', 10)

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.mjs': 'application/javascript; charset=utf-8',
  '.map': 'application/json',
  '.json': 'application/json; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
}

createServer(async (req, res) => {
  try {
    let p = decodeURIComponent(new URL(req.url, 'http://x').pathname)
    if (p === '/' || p === '') p = '/index.html'
    const filePath = join(root, normalize(p))
    if (!filePath.startsWith(root)) {
      res.writeHead(400)
      return res.end('bad path')
    }
    const data = await readFile(filePath)
    res.writeHead(200, {
      'content-type': MIME[extname(filePath).toLowerCase()] ?? 'application/octet-stream',
    })
    res.end(data)
  } catch {
    res.writeHead(404)
    res.end('not found')
  }
}).listen(port, () => {
  console.log(`serving ${root} at http://127.0.0.1:${port}`)
})
