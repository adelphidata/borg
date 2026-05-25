//! Shared helpers for panic-isolated supervised loops.

/// Convert the panic payload returned by `catch_unwind` / a `JoinError`
/// into a printable message. Most panics carry either a `&'static str`
/// or a `String`; the `unknown` fallback exists so logging never crashes
/// on an unexpected payload type.
pub fn panic_msg(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<unknown panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn static_str_payload_round_trips_through_catch_unwind() {
        // End-to-end: panic, catch it, extract the message. A regression
        // that drops the &'static str branch would surface as the unknown
        // fallback here.
        let payload = catch_unwind(AssertUnwindSafe(|| panic!("boom static")))
            .expect_err("panic must propagate");
        assert_eq!(panic_msg(&payload), "boom static");
    }

    #[test]
    fn string_payload_round_trips() {
        // `panic!("{}", value)` synthesizes a String payload — the most
        // common case for formatted panics in the agent loop.
        let payload = catch_unwind(AssertUnwindSafe(|| panic!("{}", "boom string".to_string())))
            .expect_err("panic must propagate");
        assert_eq!(panic_msg(&payload), "boom string");
    }

    #[test]
    fn unknown_payload_does_not_panic_and_returns_fallback() {
        // A non-string panic payload (e.g. `panic_any(42_i32)`) must NOT
        // crash the logger — fall back to the sentinel so supervised loops
        // never recurse into a panic during error reporting.
        let payload = catch_unwind(AssertUnwindSafe(|| std::panic::panic_any(42_i32)))
            .expect_err("panic must propagate");
        assert_eq!(panic_msg(&payload), "<unknown panic payload>");
    }
}
