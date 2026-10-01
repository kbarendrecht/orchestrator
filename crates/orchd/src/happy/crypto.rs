//! Happy's two encryption variants, and the box that wraps a key for a phone.
//!
//! **Every byte layout here is somebody else's**, so nothing in this file may be
//! tidied into a shape that reads better: the phone is the other end, and it
//! parses what `packages/happy-cli/src/api/encryption.ts` writes. The tests are
//! therefore vectors produced by *that* implementation rather than round trips of
//! this one — a round trip proves the two halves of this file agree with each
//! other, which is exactly the bug that would strand a paired phone.
//!
//! The server never sees any of this. It stores base64 of whatever comes out of
//! here and hands the same bytes to the app, which is the whole reason a third
//! client can join at all.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use crypto_box::aead::{AeadCore, OsRng};
use crypto_box::{PublicKey, SalsaBox, SecretKey};
use crypto_secretbox::XSalsa20Poly1305;

/// Which content cipher an account's credentials imply.
///
/// Not a choice this end makes. Pairing hands back either a bare 32-byte secret
/// (an older account, [`Variant::Legacy`]) or a public key to wrap per-session
/// keys for ([`Variant::DataKey`]), and the variant follows the credential for the
/// life of that pairing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// NaCl `secretbox` (XSalsa20-Poly1305) under one shared 32-byte secret.
    Legacy,
    /// AES-256-GCM under a per-session key, itself wrapped by [`seal`].
    DataKey,
}

/// XSalsa20-Poly1305 and `crypto_box` both use a 24-byte nonce.
const NONCE24: usize = 24;
/// GCM's nonce, and the one place these two layouts differ in shape.
const NONCE12: usize = 12;
/// GCM's tag, carried at the end rather than beside the ciphertext.
const TAG: usize = 16;
/// An X25519 public key, and also a `secretbox` key.
const KEY: usize = 32;

/// Encrypt `plain` for the phone.
///
/// Takes bytes rather than a value, unlike the TypeScript, which takes `any` and
/// stringifies inside. Callers here already hold `serde_json` and the split keeps
/// this file free of anything but layout.
pub fn encrypt(key: &[u8; KEY], variant: Variant, plain: &[u8]) -> Vec<u8> {
    match variant {
        Variant::Legacy => {
            // `[ nonce(24) | secretbox output ]`
            let nonce = SalsaBox::generate_nonce(&mut OsRng);
            let mut out = nonce.to_vec();
            let cipher = XSalsa20Poly1305::new(key.into());
            // The only failure `secretbox` has is an allocation one, and there is
            // nothing useful to say about it that a panic would not say louder —
            // but this crate denies panics, so an empty body is the honest answer
            // and `decrypt` on the other side reports it.
            if let Ok(mut ct) = cipher.encrypt(&nonce, plain) {
                out.append(&mut ct);
            }
            out
        }
        Variant::DataKey => {
            // `[ version(1) | nonce(12) | ciphertext | tag(16) ]`, version 0.
            //
            // GCM in `aes-gcm` returns the tag appended to the ciphertext, which
            // is already where this layout wants it — so the only assembly is the
            // version byte and the nonce in front.
            let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
            let mut out = Vec::with_capacity(1 + NONCE12 + plain.len() + TAG);
            out.push(0);
            out.extend_from_slice(&nonce);
            let cipher = Aes256Gcm::new(key.into());
            if let Ok(mut ct) = cipher.encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: &[],
                },
            ) {
                out.append(&mut ct);
            }
            out
        }
    }
}

/// Decrypt what the phone or the relay handed back, or `None`.
///
/// `None` covers a truncated blob, a wrong key and a version this build does not
/// know, deliberately without saying which: the caller's only move is the same in
/// every case, and naming the cause to a log that a relay can influence is how a
/// decryption oracle starts.
pub fn decrypt(key: &[u8; KEY], variant: Variant, blob: &[u8]) -> Option<Vec<u8>> {
    match variant {
        Variant::Legacy => {
            let (nonce, ct) = blob.split_at_checked(NONCE24)?;
            XSalsa20Poly1305::new(key.into())
                .decrypt(nonce.into(), ct)
                .ok()
        }
        Variant::DataKey => {
            // The version byte is checked rather than skipped. A future version 1
            // would keep this length and this prefix, so accepting any byte here
            // would hand GCM a body it would reject with no way to say why.
            let (head, rest) = blob.split_at_checked(1)?;
            if head.first() != Some(&0) {
                return None;
            }
            let (nonce, ct) = rest.split_at_checked(NONCE12)?;
            if ct.len() < TAG {
                return None;
            }
            Aes256Gcm::new(key.into())
                .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: &[] })
                .ok()
        }
    }
}

/// Wrap `data` for the holder of `recipient`, with a keypair used once.
///
/// This is how a per-session content key reaches the phone: the account public
/// key pairing handed back is the recipient, and the ephemeral public key rides
/// in front of the nonce so the phone needs nothing but its own secret.
///
/// Layout: `[ ephemeralPublic(32) | nonce(24) | box ciphertext ]`.
pub fn seal(data: &[u8], recipient: &[u8; KEY]) -> Vec<u8> {
    let ephemeral = SecretKey::generate(&mut OsRng);
    let nonce = SalsaBox::generate_nonce(&mut OsRng);
    let mut out = Vec::with_capacity(KEY + NONCE24 + data.len() + TAG);
    out.extend_from_slice(ephemeral.public_key().as_bytes());
    out.extend_from_slice(&nonce);
    let b = SalsaBox::new(&PublicKey::from(*recipient), &ephemeral);
    if let Ok(mut ct) = b.encrypt(&nonce, data) {
        out.append(&mut ct);
    }
    out
}

/// Open what [`seal`] wrote, with the secret half of the recipient key.
///
/// Also the shape pairing answers in: the phone seals the account credential to
/// the one-shot public key printed in the QR, and this is what opens it.
pub fn open(bundle: &[u8], secret: &[u8; KEY]) -> Option<Vec<u8>> {
    let (eph, rest) = bundle.split_at_checked(KEY)?;
    let (nonce, ct) = rest.split_at_checked(NONCE24)?;
    let eph: [u8; KEY] = eph.try_into().ok()?;
    SalsaBox::new(&PublicKey::from(eph), &SecretKey::from(*secret))
        .decrypt(nonce.into(), ct)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// Produced by `packages/happy-cli/src/api/encryption.ts` itself, over fixed
    /// keys and nonces. These are the only assertions here that can fail when
    /// *this* file is wrong in a way a round trip would not notice.
    ///
    /// `node tools/happy-vectors.mjs` regenerates them, and the same script's
    /// `verify` mode feeds blobs from [`encrypt`] and [`seal`] back through
    /// Happy's own decryptors — the direction these constants cannot test, and
    /// the one that decides whether a phone can read what orchd publishes. It is
    /// not a `mise` task because it needs an installed `happy`, which `[tools]`
    /// does not carry.
    const KEY_B64: &str = "AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dw=";
    const PLAINTEXT: &str = r#"{"t":"text","text":"hello, phone","n":42}"#;
    const LEGACY_B64: &str = "BRAbJjE8R1JdaHN+iZSfqrXAy9bh7PcCv8yr9hJM5tK48AVC/SSlHvjNR/VUlLSopuoCrWeHGBkjAisOwAzNuImlYQMXtffiyGH6htmh0Jx0";
    const DATAKEY_B64: &str = "AAkWIzA9SldkcX6LmPQwTv2am7Ixo77UkqwyPOL6coc3+0+UGEheZ9Ii3S6eVSXo04N9zPzD5samLTXmp0utrrGO5NU70Q==";
    const BOX_SECRET_B64: &str = "AgcMERYbICUqLzQ5PkNITVJXXGFma3B1en+EiY6TmJ0=";
    const BOX_BUNDLE_B64: &str = "w3B3+0MtGcUdaoxv352/BvhHHPEu2xytq3aurCmReh4FEBsmMTxHUl1oc36JlJ+qtcDL1uHs9wLNIp2A/PVQbGlnNkKZUwwRpOUFYWVnByDNaT/uyLKVg7UFMUoFWq8y90EOeOHnwVk=";

    fn d(s: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    fn key32(s: &str) -> [u8; 32] {
        d(s).try_into().unwrap()
    }

    #[test]
    fn a_legacy_blob_written_by_happy_reads_here() {
        let out = decrypt(&key32(KEY_B64), Variant::Legacy, &d(LEGACY_B64)).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PLAINTEXT);
    }

    #[test]
    fn a_datakey_blob_written_by_happy_reads_here() {
        let out = decrypt(&key32(KEY_B64), Variant::DataKey, &d(DATAKEY_B64)).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), PLAINTEXT);
    }

    /// The pairing keypair is raw — `box.keyPair.fromSecretKey(randomBytes(32))`
    /// in `doAuth` — not the libsodium sha512-of-a-seed derivation that
    /// `libsodiumPublicKeyFromSecretKey` exists for. Both spellings are in
    /// Happy's source and only this one is on orchd's path.
    #[test]
    fn a_sealed_key_written_by_happy_opens_here() {
        let out = open(&d(BOX_BUNDLE_B64), &key32(BOX_SECRET_B64)).unwrap();
        assert_eq!(out, d(KEY_B64));
    }

    #[test]
    fn both_variants_come_back_out_of_their_own_cipher() {
        for v in [Variant::Legacy, Variant::DataKey] {
            let k = key32(KEY_B64);
            let blob = encrypt(&k, v, PLAINTEXT.as_bytes());
            assert_eq!(decrypt(&k, v, &blob).unwrap(), PLAINTEXT.as_bytes());
        }
    }

    #[test]
    fn a_sealed_key_comes_back_to_the_holder_of_the_secret() {
        let secret = SecretKey::generate(&mut OsRng);
        let pubkey: [u8; 32] = *secret.public_key().as_bytes();
        let sealed = seal(b"a data key", &pubkey);
        assert_eq!(open(&sealed, &secret.to_bytes()).unwrap(), b"a data key");
    }

    /// The three ways a blob can be wrong, each answered `None` rather than a
    /// panic: the daemon decrypts whatever the relay sends it.
    #[test]
    fn a_wrong_key_a_short_blob_and_a_strange_version_all_answer_none() {
        let k = key32(KEY_B64);
        let mut wrong = k;
        wrong[0] ^= 1;
        assert!(decrypt(&wrong, Variant::DataKey, &d(DATAKEY_B64)).is_none());
        assert!(decrypt(&wrong, Variant::Legacy, &d(LEGACY_B64)).is_none());
        for n in 0..14 {
            assert!(decrypt(&k, Variant::DataKey, &d(DATAKEY_B64)[..n]).is_none());
            assert!(decrypt(&k, Variant::Legacy, &d(LEGACY_B64)[..n]).is_none());
        }
        let mut v1 = d(DATAKEY_B64);
        v1[0] = 1;
        assert!(decrypt(&k, Variant::DataKey, &v1).is_none());
        assert!(open(&d(BOX_BUNDLE_B64)[..40], &k).is_none());
    }
}
