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
