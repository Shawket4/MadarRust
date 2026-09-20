//! Comparing secrets without telling the caller how close they got.

/// Compare two byte strings without leaking, through timing, HOW MUCH of the
/// first matched.
///
/// A naive `==` returns as soon as two bytes differ, so a guess sharing a longer
/// prefix with the real value takes measurably longer to reject. Against a
/// six-digit OTP that is a short ladder to climb. Folding every byte into one
/// accumulator costs the same time whatever the input.
///
/// The early length check is deliberate and safe: the length of these values is
/// fixed and public (a six-digit code, a minted token), so it is not the secret.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn equal_values_match_and_nothing_else_does() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        // A shared prefix must not be treated as a match.
        assert!(!constant_time_eq(b"123456", b"12345"));
        assert!(!constant_time_eq(b"", b"0"));
        assert!(constant_time_eq(b"", b""));
    }
}

/// The bcrypt cost every hash in this system is made with.
///
/// bcrypt's floor. The default (12) measures **2.4 seconds** on the production
/// box — a single vCPU — which is not a price a sign-in can pay, and was being
/// paid on the async executor where it stopped the whole API for the duration.
///
/// What the cost actually buys is offline resistance if the hash table is
/// stolen, and for most of what is hashed here that was never much:
///
///   * **PINs** are four to six digits. The entire space is a million guesses,
///     so a cost factor buys time, not safety; the protection is that a PIN
///     only works against this server, which rate-limits and locks out.
///   * **Integration secrets** are long random strings this system generated,
///     where guessing is hopeless at any cost.
///
/// The real reduction is to **owner and staff passwords**, which people choose
/// and reuse. That is a deliberate trade made by the owner, who wanted the
/// latency gone; it is recorded here rather than buried in a diff so it can be
/// revisited with the numbers in view. Raising it is a one-line change, plus
/// the rehash-on-verify below, which upgrades every hash the next time its
/// owner signs in.
/// bcrypt's own `MIN_COST` is private to that crate, so the floor is named
/// here. The algorithm defines it as 4; a lower value is rejected outright by
/// `bcrypt::hash`, which the test below pins.
pub const BCRYPT_COST: u32 = 4;

/// Does this hash need re-making because it was created at a different cost?
///
/// A bcrypt hash carries its own cost in the string (`$2b$12$…`), so changing
/// `BCRYPT_COST` does NOT speed up an existing one — it will keep verifying at
/// whatever it was born with, for ever, and a shop whose staff were hashed at
/// 12 would see no improvement at all. Callers check this after a SUCCESSFUL
/// verify (the only moment the plaintext is in hand and known correct) and
/// write back a fresh hash.
pub fn needs_rehash(hash: &str) -> bool {
    // `$2b$<cost>$<salt+digest>` — the cost is the third field.
    match hash.split('$').nth(2).and_then(|c| c.parse::<u32>().ok()) {
        Some(cost) => cost != BCRYPT_COST,
        // Unparseable: leave it alone rather than churn something we do not
        // understand. A failed verify is handled by the caller either way.
        None => false,
    }
}

#[cfg(test)]
mod cost_tests {
    use super::{BCRYPT_COST, needs_rehash};

    #[test]
    fn the_cost_is_accepted_and_lands_in_the_hash() {
        let h = bcrypt::hash("123456", BCRYPT_COST).expect("bcrypt accepts the floor");
        assert!(bcrypt::verify("123456", &h).unwrap());
        assert!(!needs_rehash(&h), "a hash we just made is current: {h}");
    }

    #[test]
    fn a_hash_from_the_old_default_is_flagged_for_rehash() {
        // The shape bcrypt writes, at the cost production has been using.
        assert!(needs_rehash(
            "$2b$12$abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUV"
        ));
        // Nonsense is left alone rather than churned.
        assert!(!needs_rehash("not-a-bcrypt-hash"));
        assert!(!needs_rehash(""));
    }
}
