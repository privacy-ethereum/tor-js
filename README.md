# `keccak` — hash-addressed objects

This branch is not code. It is a content store: a flat set of immutable objects,
each named for the keccak256 of its own bytes, which
[tor-js-gateway](https://github.com/privacy-ethereum/tor-js/tree/main/crates/tor-js-gateway)
instances mirror and serve at `GET /keccak/{hh}/{rest}` over KPS.

It has no shared history with `main` — deliberately, so cloning it costs the
objects and nothing else.

## Layout

```
<hh>/<rest>
```

`<hh>` is the first 2 and `<rest>` the remaining 62 of the object's 64 lowercase
hex chars of `keccak256(bytes)`, with no `0x` and **no file extension**: the
whole path is the hash. Anything else in this branch — this README, the
`.gitattributes` — is ignored by mirrors.

## Contents

| Object | What it is |
|---|---|
| `23/32139f37b1e2c7a9713509f2bc2b48c71e89c2a20822472706bfa0a7ba2f57` | The tor-js 0.4.1 anon-rpc worker bundle (5,948,460 bytes) — `dist/anon-rpc-worker.js` as published in [`tor-js@0.4.1`](https://www.npmjs.com/package/tor-js/v/0.4.1). Pinned on Ethereum mainnet by the `WorkerSpecifier` at [`0x700dA3193D35fA54Cd3fBf29B66f2a2A0385659e`](https://etherscan.io/address/0x700dA3193D35fA54Cd3fBf29B66f2a2A0385659e). |

## Publishing

Commit the file under its own hash and push:

```sh
hash=$(node -e 'const{keccak_256}=require("@noble/hashes/sha3");process.stdout.write(Buffer.from(keccak_256(require("fs").readFileSync(process.argv[1]))).toString("hex"))' worker.js)
mkdir -p "${hash:0:2}"
cp worker.js "${hash:0:2}/${hash:2}"
git add "${hash:0:2}/${hash:2}"
git commit -m "publish $hash"
git push origin keccak
```

Deleting a file unpublishes that object: mirrors remove it on their next sync
and stop serving it.

Gateways poll roughly daily, so a push is visible within a day. To make one
pick it up immediately, ask it directly over KPS:

```
POST /keccak/sync
```

which answers once the sync has finished. It is refused with `429` and a
`Retry-After` if a client-triggered sync ran there in the last 30 minutes.

## Verifying an object

Nothing here is trusted on the basis of where it came from. Every consumer
re-derives the name from the bytes, and so can you:

```sh
node -e 'const{keccak_256}=require("@noble/hashes/sha3");console.log(Buffer.from(keccak_256(require("fs").readFileSync(process.argv[1]))).toString("hex"))' \
  23/32139f37b1e2c7a9713509f2bc2b48c71e89c2a20822472706bfa0a7ba2f57
```

The output must equal the path with the `/` removed. A mirror refuses to store
an object whose contents disagree with its name, and refuses to serve one whose
contents have since changed — so a file whose name lies is inert, not dangerous.
