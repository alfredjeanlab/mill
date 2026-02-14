use serde::Serialize;

/// Print a value as pretty JSON to stdout.
pub fn print_json<T: Serialize + ?Sized>(value: &T) {
    if let Ok(s) = serde_json::to_string_pretty(value) {
        println!("{s}");
    }
}

/// Print a value as a single JSON line to stdout (for SSE events).
pub fn print_json_line<T: Serialize>(value: &T) {
    if let Ok(s) = serde_json::to_string(value) {
        println!("{s}");
    }
}
