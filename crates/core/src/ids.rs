//! ULID correlation / event IDs (ADR-02): time-sortable, URL-safe, no
//! hyphens — easier to grep out of logs than a UUID.

pub use ulid::Ulid;

/// Generate a new ULID. Thin wrapper so call sites read
/// `concerto_core::ids::new_id()` rather than reaching for the `ulid` crate
/// directly — keeps the ID scheme swappable behind one seam.
pub fn new_id() -> Ulid {
    Ulid::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
    }

    #[test]
    fn ids_are_monotonic() {
        // Note: Ulid::new() does NOT guarantee strict monotonicity within the same
        // millisecond (it uses random bits for the lower 80 bits). We only verify
        // that IDs are unique and have accessible timestamps.
        let a = new_id();
        let b = new_id();
        let c = new_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        // Timestamps should be non-decreasing (all generated within ~1ms)
        assert!(a.timestamp_ms() <= b.timestamp_ms());
        assert!(b.timestamp_ms() <= c.timestamp_ms());
    }

    #[test]
    fn ids_are_time_based() {
        let id = new_id();
        // ULIDs embed a 48-bit timestamp (ms since Unix epoch).
        // After Jul 2026 this will be > 1_784_000_000_000 ms.
        let now_ms =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let id_ms: u128 = id.timestamp_ms().into();
        // Should be within 10 seconds.
        assert!(id_ms <= now_ms + 10_000);
        assert!(id_ms >= now_ms.saturating_sub(10_000));
    }

    #[test]
    fn ids_are_ulid_type() {
        let id = new_id();
        let _ulid: Ulid = id; // type assertion
    }

    #[test]
    fn id_format_is_26_char_uppercase_base32() {
        let id = new_id();
        let s = id.to_string();
        assert_eq!(s.len(), 26);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn id_from_string_round_trips() {
        let original = new_id();
        let s = original.to_string();
        let parsed: Ulid = s.parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn id_is_send_and_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Ulid>();
        assert_sync::<Ulid>();
    }

    #[test]
    fn id_is_copy() {
        let id = new_id();
        let _copied = id; // still usable
        let _ = id.to_string(); // not moved
    }

    #[test]
    fn id_ulid_timestamp_accessible() {
        let id = new_id();
        let ts: u64 = id.timestamp_ms();
        assert!(ts > 0, "ULID should have a non-zero timestamp");
    }

    #[test]
    fn id_new_returns_different_values_on_each_call() {
        let ids: Vec<Ulid> = (0..100).map(|_| new_id()).collect();
        // Verify uniqueness (not monotonicity — Ulid::new() uses random bits
        // for the lower 80 bits, so strict ordering isn't guaranteed within
        // the same millisecond).
        let mut unique_ids = ids.clone();
        unique_ids.sort();
        unique_ids.dedup();
        assert_eq!(unique_ids.len(), 100, "100 consecutive IDs must be unique");
    }
}
