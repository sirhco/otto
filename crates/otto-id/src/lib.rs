//! Sortable, monotonic identifiers — a byte-faithful port of opencode's
//! `packages/opencode/src/id/id.ts`.
//!
//! An identifier has the shape `<prefix>_<time+counter hex><base62 random>`.
//! Mirroring `id.ts`, the segment after the underscore is always
//! [`LENGTH`] (26) characters: a 12-character hex encoding of a 48-bit
//! `time*0x1000 + counter` value, followed by 14 base62 random characters.
//!
//! * **Ascending** ids sort lexicographically in chronological / generation
//!   order.
//! * **Descending** ids store the bitwise complement of the 48-bit time
//!   segment, so they sort in reverse-chronological order.
//!
//! The monotonic counter (12 bits worth, `0x1000`) guarantees that within a
//! single millisecond successive calls still sort in generation order,
//! exactly as `id.ts` does with its module-global `lastTimestamp`/`counter`.

use std::sync::Mutex;

use rand::RngCore;

/// Length of the encoded portion following `<prefix>_`.
///
/// Matches `const LENGTH = 26` in `id.ts`. It is split into a 12-char hex
/// time+counter segment and a `LENGTH - 12 = 14` char base62 random segment.
const LENGTH: usize = 26;

/// Number of hex characters used for the time+counter segment.
///
/// `id.ts` writes 6 bytes (`Buffer.alloc(6)`), i.e. 48 bits, as hex → 12 chars.
const TIME_HEX_LEN: usize = 12;

/// The base62 alphabet, identical (order included) to the one in `id.ts`.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Mask selecting the low 48 bits emitted as the time+counter segment.
///
/// `id.ts` extracts exactly 6 bytes (`(now >> (40 - 8*i)) & 0xff`), discarding
/// any higher bits of `now`.
const MASK48: u64 = 0xFFFF_FFFF_FFFF;

/// Known identifier prefixes.
///
/// The string values match the `prefixes` object in `id.ts` byte-for-byte.
/// Note that opencode names the worktree/workspace key `workspace` with the
/// prefix string `"wrk"`; the [`Prefix::Workspace`] variant preserves that
/// exact prefix string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    /// `job` — background jobs.
    Job,
    /// `evt` — events.
    Event,
    /// `ses` — sessions.
    Session,
    /// `msg` — messages.
    Message,
    /// `per` — permissions.
    Permission,
    /// `que` — questions.
    Question,
    /// `prt` — message parts.
    Part,
    /// `pty` — pseudo-terminals.
    Pty,
    /// `tool` — tools.
    Tool,
    /// `wrk` — worktree/workspace (opencode key `workspace`).
    Workspace,
}

impl Prefix {
    /// Returns the prefix string, matching `prefixes[...]` in `id.ts`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Prefix::Job => "job",
            Prefix::Event => "evt",
            Prefix::Session => "ses",
            Prefix::Message => "msg",
            Prefix::Permission => "per",
            Prefix::Question => "que",
            Prefix::Part => "prt",
            Prefix::Pty => "pty",
            Prefix::Tool => "tool",
            Prefix::Workspace => "wrk",
        }
    }
}

/// Sort direction of a generated id.
///
/// Corresponds to the `"ascending" | "descending"` union in `id.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Chronological order: later ids sort lexicographically greater.
    Ascending,
    /// Reverse-chronological order: the 48-bit time segment is bitwise
    /// inverted (`~now` in `id.ts`) so later ids sort lexicographically
    /// smaller.
    Descending,
}

/// Module-global generator state, mirroring `id.ts`'s `lastTimestamp`/`counter`.
struct GenState {
    last_timestamp: u64,
    counter: u64,
}

static STATE: Mutex<GenState> = Mutex::new(GenState {
    last_timestamp: 0,
    counter: 0,
});

/// Current wall-clock time in milliseconds since the Unix epoch.
///
/// Equivalent to `Date.now()` in `id.ts`.
fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as u64
}

/// Generates an ascending (chronologically sortable) id for `prefix`.
///
/// Equivalent to `ascending(prefix)` in `id.ts`.
#[must_use]
pub fn ascending(prefix: Prefix) -> String {
    create(prefix.as_str(), Direction::Ascending, None)
}

/// Generates a descending (reverse-chronologically sortable) id for `prefix`.
///
/// Equivalent to `descending(prefix)` in `id.ts`.
#[must_use]
pub fn descending(prefix: Prefix) -> String {
    create(prefix.as_str(), Direction::Descending, None)
}

/// Generates an id for `prefix` in the given `direction`.
///
/// Convenience wrapper combining [`ascending`] and [`descending`].
#[must_use]
pub fn generate(prefix: Prefix, direction: Direction) -> String {
    create(prefix.as_str(), direction, None)
}

/// Core id builder — a direct port of `create()` in `id.ts`.
///
/// `timestamp` mirrors the optional `timestamp?` parameter: when `None` the
/// current time ([`now_millis`]) is used. The monotonic counter is reset
/// whenever the millisecond changes and incremented on every call, then packed
/// as `timestamp * 0x1000 + counter`. For [`Direction::Descending`] the 48-bit
/// value is bitwise inverted (`~now`) before encoding.
///
/// The result is `prefix + "_" + <12 hex chars> + <14 base62 chars>`.
#[must_use]
pub fn create(prefix: &str, direction: Direction, timestamp: Option<u64>) -> String {
    let current = timestamp.unwrap_or_else(now_millis);

    let counter = {
        let mut state = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current != state.last_timestamp {
            state.last_timestamp = current;
            state.counter = 0;
        }
        state.counter += 1;
        state.counter
    };

    // now = BigInt(currentTimestamp) * 0x1000 + counter, then keep low 48 bits.
    let now = (current as u128) * 0x1000 + counter as u128;
    let mut low48 = (now & MASK48 as u128) as u64;
    if matches!(direction, Direction::Descending) {
        // JS `~now` inverts every bit; only the low 48 bits are ever emitted,
        // so inverting the masked value is equivalent.
        low48 = !low48 & MASK48;
    }

    let mut id = String::with_capacity(prefix.len() + 1 + LENGTH);
    id.push_str(prefix);
    id.push('_');

    // 6 big-endian bytes (bits 40..0) as lowercase hex — matches
    // `timeBytes[i] = (now >> (40 - 8*i)) & 0xff` + `Buffer.toString("hex")`.
    for byte in &low48.to_be_bytes()[2..] {
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").expect("writing to String is infallible");
    }

    // randomBase62(LENGTH - 12): random bytes, each taken `% 62`.
    let mut buf = [0u8; LENGTH - TIME_HEX_LEN];
    rand::thread_rng().fill_bytes(&mut buf);
    for byte in buf {
        id.push(ALPHABET[(byte % 62) as usize] as char);
    }

    id
}

/// Extracts the millisecond timestamp encoded in an **ascending** id.
///
/// Direct port of `timestamp(id)` in `id.ts`: it reads the 12 hex characters
/// after `<prefix>_`, parses them as a 48-bit integer, and divides by `0x1000`
/// to drop the counter. Returns `None` if the id is malformed (no underscore,
/// too short, or non-hex time segment).
///
/// Note: because only 48 bits are stored, the recovered value equals
/// `timestamp % 2^36` for millisecond timestamps past year ~1972. This
/// truncation is faithful to `id.ts`; descending ids are not supported.
#[must_use]
pub fn timestamp(id: &str) -> Option<u64> {
    let prefix = id.split_once('_')?.0;
    let start = prefix.len() + 1;
    let hex = id.get(start..start + TIME_HEX_LEN)?;
    let encoded = u64::from_str_radix(hex, 16).ok()?;
    Some(encoded / 0x1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that touch the module-global generator state so the
    /// deterministic counter assertions are not disturbed by parallel tests.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn is_base62(s: &str) -> bool {
        s.bytes().all(|b| ALPHABET.contains(&b))
    }

    fn is_hex(s: &str) -> bool {
        s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn format_prefix_and_lengths() {
        let _g = serial();
        for prefix in [
            Prefix::Job,
            Prefix::Event,
            Prefix::Session,
            Prefix::Message,
            Prefix::Permission,
            Prefix::Question,
            Prefix::Part,
            Prefix::Pty,
            Prefix::Tool,
            Prefix::Workspace,
        ] {
            let id = ascending(prefix);
            let expected_prefix = format!("{}_", prefix.as_str());
            assert!(id.starts_with(&expected_prefix), "bad prefix: {id}");

            let rest = &id[expected_prefix.len()..];
            assert_eq!(rest.len(), LENGTH, "encoded portion must be 26 chars");

            let (time_seg, rand_seg) = rest.split_at(TIME_HEX_LEN);
            assert_eq!(time_seg.len(), 12);
            assert_eq!(rand_seg.len(), 14);
            assert!(is_hex(time_seg), "time segment must be hex: {time_seg}");
            assert!(is_base62(rand_seg), "random segment must be base62");
        }
    }

    #[test]
    fn descending_has_same_shape() {
        let _g = serial();
        let id = descending(Prefix::Session);
        assert!(id.starts_with("ses_"));
        assert_eq!(id.len(), "ses_".len() + LENGTH);
    }

    #[test]
    fn random_segment_only_uses_base62() {
        let _g = serial();
        for _ in 0..1000 {
            let id = ascending(Prefix::Message);
            let rand_seg = &id["msg_".len() + TIME_HEX_LEN..];
            assert!(is_base62(rand_seg), "non-base62 char in {rand_seg}");
        }
    }

    #[test]
    fn prefix_strings_match_id_ts() {
        assert_eq!(Prefix::Job.as_str(), "job");
        assert_eq!(Prefix::Event.as_str(), "evt");
        assert_eq!(Prefix::Session.as_str(), "ses");
        assert_eq!(Prefix::Message.as_str(), "msg");
        assert_eq!(Prefix::Permission.as_str(), "per");
        assert_eq!(Prefix::Question.as_str(), "que");
        assert_eq!(Prefix::Part.as_str(), "prt");
        assert_eq!(Prefix::Pty.as_str(), "pty");
        assert_eq!(Prefix::Tool.as_str(), "tool");
        assert_eq!(Prefix::Workspace.as_str(), "wrk");
    }

    #[test]
    fn timestamp_roundtrips_for_small_values() {
        let _g = serial();
        // Any ms value < 2^36 fits the 48-bit segment without truncation, so
        // it round-trips exactly (a single call keeps counter < 0x1000).
        let ts = 987_654_321u64;
        let id = create(Prefix::Session.as_str(), Direction::Ascending, Some(ts));
        assert_eq!(timestamp(&id), Some(ts));
    }

    #[test]
    fn timestamp_truncates_large_values_like_id_ts() {
        let _g = serial();
        // Value observed from the reference implementation in Node.
        let ts = 1_782_967_209_067u64;
        let id = create(Prefix::Session.as_str(), Direction::Ascending, Some(ts));
        let decoded = timestamp(&id).unwrap();
        // Faithful to id.ts: only 48 bits are stored → timestamp % 2^36.
        assert_eq!(decoded, ts % (1u64 << 36));
        assert_eq!(decoded, 64_980_290_667);
        assert_ne!(decoded, ts, "large timestamps are truncated, as in id.ts");
    }

    #[test]
    fn timestamp_rejects_malformed_ids() {
        assert_eq!(timestamp("nounderscorehere"), None);
        assert_eq!(timestamp("ses_short"), None);
        assert_eq!(timestamp("ses_zzzzzzzzzzzz00000000000000"), None);
    }

    #[test]
    fn ascending_ids_are_generated_in_sorted_order() {
        let _g = serial();
        // Fixed timestamp: counter increments 1..=N, so the packed value is
        // strictly increasing and later ids are lexicographically greater.
        const N: usize = 10_000;
        let ids: Vec<String> = (0..N)
            .map(|_| create(Prefix::Session.as_str(), Direction::Ascending, Some(1)))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ascending ids must be strictly increasing"
        );
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "ascending ids must already be sorted");
    }

    #[test]
    fn descending_ids_are_generated_in_reverse_sorted_order() {
        let _g = serial();
        const N: usize = 10_000;
        let ids: Vec<String> = (0..N)
            .map(|_| create(Prefix::Session.as_str(), Direction::Descending, Some(1)))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] > w[1]),
            "descending ids must be strictly decreasing"
        );
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.reverse();
        assert_eq!(ids, sorted, "descending ids must be reverse-sorted");
    }

    #[test]
    fn ascending_and_descending_order_oppositely() {
        let _g = serial();
        let ts = 555_555u64;
        let a0 = create(Prefix::Session.as_str(), Direction::Ascending, Some(ts));
        let a1 = create(Prefix::Session.as_str(), Direction::Ascending, Some(ts));
        assert!(a0 < a1, "ascending: earlier < later");

        let d0 = create(Prefix::Session.as_str(), Direction::Descending, Some(ts));
        let d1 = create(Prefix::Session.as_str(), Direction::Descending, Some(ts));
        assert!(d0 > d1, "descending: earlier > later");
    }

    #[test]
    fn descending_is_bitwise_complement_of_ascending() {
        let _g = serial();
        let ts = 42u64;
        let asc = create(Prefix::Session.as_str(), Direction::Ascending, Some(ts));
        let asc_hex = &asc["ses_".len().."ses_".len() + TIME_HEX_LEN];
        let asc_val = u64::from_str_radix(asc_hex, 16).unwrap();

        let desc = create(Prefix::Session.as_str(), Direction::Descending, Some(ts));
        let desc_hex = &desc["ses_".len().."ses_".len() + TIME_HEX_LEN];
        let desc_val = u64::from_str_radix(desc_hex, 16).unwrap();

        // asc used counter n; desc used counter n+1. The complement of the
        // descending packed value equals asc's packed value + 1 (next counter).
        assert_eq!(!desc_val & MASK48, asc_val + 1);
    }
}
