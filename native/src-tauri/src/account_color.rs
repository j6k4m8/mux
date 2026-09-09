//! Which of the six palette colors a new account gets.
//!
//! Assigning by creation order meant an IMAP account's color shifted
//! whenever another account was added or removed before it, and Gmail never
//! counted at all — every Gmail account landed on the same color. Hashing the
//! account's own identity fixes both: the same account always lands on the
//! same color no matter what else exists, and two different accounts
//! collide on a color about as often as any hash does — one in six here,
//! which is not worse than picking at random and is a great deal more stable.

/// The same six colors `native/src/accountColor.ts` offers in Settings, in
/// the same order; a Rust test holds the two together.
pub(crate) const ACCOUNT_COLORS: [&str; 6] = [
    "#5168f4", "#12a58c", "#b3730a", "#c93b63", "#7c4ddb", "#0a6fa8",
];

/// FNV-1a over the account's own identity, picked down to one of six buckets.
/// No dependency, deterministic across runs and platforms; this is not trying
/// to be cryptographic, only stable and evenly spread.
pub(crate) fn account_color_for(identity: &str) -> &'static str {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    ACCOUNT_COLORS[(hash % ACCOUNT_COLORS.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_identity_always_lands_on_the_same_colour() {
        assert_eq!(
            account_color_for("imap:abc123"),
            account_color_for("imap:abc123")
        );
    }

    #[test]
    fn every_colour_offered_is_one_of_the_six() {
        for identity in ["a", "reader@example.test", "imap:0011", "", "z"] {
            assert!(ACCOUNT_COLORS.contains(&account_color_for(identity)));
        }
    }

    #[test]
    fn different_identities_are_not_all_dealt_the_same_colour() {
        // Regression: this is exactly the bug being fixed — every Gmail
        // account landed on '#5168f4' because nothing about the account fed
        // the choice. A handful of distinct identities should not collapse
        // onto one color.
        let colors: std::collections::HashSet<&str> = [
            "gmail:reader@one.example",
            "gmail:reader@two.example",
            "gmail:reader@three.example",
            "gmail:reader@four.example",
        ]
        .iter()
        .map(|identity| account_color_for(identity))
        .collect();
        assert!(colors.len() > 1);
    }
}
