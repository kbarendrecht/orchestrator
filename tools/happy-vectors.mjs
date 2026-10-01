#!/usr/bin/env node
// Interop vectors for `crates/orchd/src/happy/crypto.rs`, from Happy's own code.
//
//   node tools/happy-vectors.mjs                 print the constants
//   node tools/happy-vectors.mjs verify A B C    read blobs orchd produced
//
// **A round trip proves nothing here.** Both halves of crypto.rs can agree with
// each other and disagree with the phone, and the symptom is a paired device that
// shows an empty session — so the constants in that file's tests come from
// `packages/happy-cli/src/api/encryption.ts`, ported verbatim below, and the
// `verify` mode runs the other direction: blobs orchd wrote, through Happy's own
// decryptors. Neither direction can be a `mise` task, because both need an
// installed `happy` and `[tools]` does not carry one.
//
// Fixed keys and nonces, so the printed constants are stable across runs.
import { createCipheriv, createDecipheriv } from 'node:crypto'
import { createRequire } from 'node:module'
import { execFileSync } from 'node:child_process'

// tweetnacl comes from wherever `happy` is installed; there is no copy in
// `tools/node_modules` and adding one would be a second implementation to drift.
function nacl() {
  const bin = execFileSync('sh', ['-c', 'command -v happy'], { encoding: 'utf8' }).trim()
  const root = execFileSync('node', ['-e', `console.log(require('node:fs').realpathSync('${bin}'))`], { encoding: 'utf8' }).trim()
  const modules = root.slice(0, root.indexOf('/node_modules/') + '/node_modules/'.length)
  return createRequire(modules).call(null, 'tweetnacl')
}

const b64 = (u) => Buffer.from(u).toString('base64')
const dec = (s) => new Uint8Array(Buffer.from(s, 'base64'))
const ramp = (n, a, b) => new Uint8Array(n).map((_, i) => (i * a + b) & 0xff)

const KEY = ramp(32, 7, 3)
const NONCE24 = ramp(24, 11, 5)
const NONCE12 = ramp(12, 13, 9)
const BOX_SECRET = ramp(32, 5, 2)
const EPH_SECRET = ramp(32, 3, 1)
const DATA = { t: 'text', text: 'hello, phone', n: 42 }
const PLAIN = new TextEncoder().encode(JSON.stringify(DATA))

function generate(n) {
  // encryptLegacy: [ nonce(24) | secretbox output ]
  const lct = n.secretbox(PLAIN, NONCE24, KEY)
  const legacy = new Uint8Array(24 + lct.length)
  legacy.set(NONCE24); legacy.set(lct, 24)

  // encryptWithDataKey: [ version(1) | nonce(12) | ciphertext | tag(16) ]
  const c = createCipheriv('aes-256-gcm', KEY, NONCE12)
  const ct = Buffer.concat([c.update(PLAIN), c.final()])
  const tag = c.getAuthTag()
  const dataKey = new Uint8Array(1 + 12 + ct.length + 16)
  dataKey.set([0], 0); dataKey.set(NONCE12, 1); dataKey.set(ct, 13); dataKey.set(tag, 13 + ct.length)

  // libsodiumEncryptForPublicKey: [ ephemeralPublic(32) | nonce(24) | box output ].
  // The recipient keypair is raw, the way `doAuth` makes the one-shot pairing
  // pair — not the libsodium sha512-of-a-seed derivation, which is a different
  // entry point and not one orchd uses.
  const eph = n.box.keyPair.fromSecretKey(EPH_SECRET)
  const recip = n.box.keyPair.fromSecretKey(BOX_SECRET)
  const boxed = n.box(KEY, NONCE24, recip.publicKey, eph.secretKey)
  const bundle = new Uint8Array(32 + 24 + boxed.length)
  bundle.set(eph.publicKey, 0); bundle.set(NONCE24, 32); bundle.set(boxed, 56)

  console.log(`const KEY_B64: &str = "${b64(KEY)}";`)
  console.log(`const PLAINTEXT: &str = r#"${JSON.stringify(DATA)}"#;`)
  console.log(`const LEGACY_B64: &str = "${b64(legacy)}";`)
  console.log(`const DATAKEY_B64: &str = "${b64(dataKey)}";`)
  console.log(`const BOX_SECRET_B64: &str = "${b64(BOX_SECRET)}";`)
  console.log(`const BOX_BUNDLE_B64: &str = "${b64(bundle)}";`)
}

function verify(n, legacyB64, dataKeyB64, boxB64) {
  // decryptLegacy
  const lb = dec(legacyB64)
  const legacy = n.secretbox.open(lb.slice(24), lb.slice(0, 24), KEY)
  console.log('legacy  :', legacy ? new TextDecoder().decode(legacy) : 'FAILED')

  // decryptWithDataKey
  const b = dec(dataKeyB64)
  if (b[0] !== 0) throw new Error('version byte is not 0')
  const d = createDecipheriv('aes-256-gcm', KEY, b.slice(1, 13))
  d.setAuthTag(b.slice(b.length - 16))
  const out = Buffer.concat([d.update(b.slice(13, b.length - 16)), d.final()])
  console.log('dataKey :', new TextDecoder().decode(out))

  // decryptWithEphemeralKey
  const bb = dec(boxB64)
  const opened = n.box.open(bb.slice(56), bb.slice(32, 56), bb.slice(0, 32), BOX_SECRET)
  console.log('box     :', opened ? b64(opened) : 'FAILED')
}

const [mode, ...rest] = process.argv.slice(2)
const n = nacl()
if (mode === 'verify') {
  if (rest.length !== 3) {
    console.error('verify wants three base64 blobs: legacy, dataKey, box')
    process.exit(2)
  }
  verify(n, ...rest)
} else {
  generate(n)
}
