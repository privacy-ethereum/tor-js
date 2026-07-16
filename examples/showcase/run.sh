#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")"
GIT_ROOT=$(git rev-parse --show-toplevel)

if [ ! -d "$GIT_ROOT/dist" ]; then
    echo "Error: package dist not found at $GIT_ROOT/dist"
    echo "Run: npm run build"
    exit 1
fi

rm -rf dist
cp -a "$GIT_ROOT"/dist dist
echo "*" >dist/.gitignore

# Serve with caching fully disabled. `python3 -m http.server` answers
# If-Modified-Since with a 304 based only on file mtime (no ETag, no
# Cache-Control), so embedded browsers (e.g. VS Code's Simple Browser) keep
# running stale ES modules after a rebuild. Strip conditional request headers
# and send no-store so every reload gets fresh bytes.
python3 -c '
import http.server, socketserver

class Handler(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        for h in ("If-Modified-Since", "If-None-Match"):
            while h in self.headers:
                del self.headers[h]
        return super().send_head()

    def end_headers(self):
        self.send_header("Cache-Control", "no-store, no-cache, must-revalidate, max-age=0")
        self.send_header("Expires", "0")
        super().end_headers()

socketserver.ThreadingTCPServer.allow_reuse_address = True
with socketserver.ThreadingTCPServer(("", 8000), Handler) as httpd:
    print("Serving on http://localhost:8000 (caching disabled)")
    httpd.serve_forever()
'
