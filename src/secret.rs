//! A fresh secret, and nothing else.
//!
//! **Its own module because it is a leaf, and a leaf is what breaks a cycle.**
//! This function lived in [`crate::state`], where it was reached by five modules
//! including [`crate::model`] — and `state` reads `model` for every type it
//! stores, so those two imported each other over one call to a three-line
//! function with no state of its own. `mise run check-modules` counted that as
//! one of seventeen such pairs; this is the first one taken out.
//!
//! Nothing here may grow a dependency. The moment this module imports another
//! one it stops being a leaf, and whichever cycle it is standing between comes
//! back.

/// A fresh secret: 32 lowercase hex characters, 122 random bits.
///
/// A v4 uuid rather than a second RNG dependency: `uuid` already draws from the
/// OS RNG for every session id, and nothing anywhere parses the token's shape,
/// only compares it whole.
pub fn random_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a test of the RNG — of the two properties every caller relies on: it
    /// is a comparable string of a fixed shape, and two calls differ.
    #[test]
    fn a_token_is_32_hex_characters_and_never_repeats() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 32, "{a} is not 32 characters");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }
}
