pub mod json;
pub mod table;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

/// Print a serializable value according to the output mode.
/// For Human mode, uses the provided display function.
/// For Json mode, serializes to pretty JSON.
pub fn print<T, F>(mode: OutputMode, value: &T, human_fn: F)
where
    T: Serialize,
    F: FnOnce(&T),
{
    match mode {
        OutputMode::Human => human_fn(value),
        OutputMode::Json => json::print_json(value),
    }
}

/// Print a table (human) or JSON array (json mode).
pub fn print_list<T, F>(mode: OutputMode, items: &[T], human_fn: F)
where
    T: Serialize,
    F: FnOnce(&[T]),
{
    match mode {
        OutputMode::Human => human_fn(items),
        OutputMode::Json => json::print_json(items),
    }
}
