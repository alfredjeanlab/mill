use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

/// An SSE event to be sent to clients.
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Convert an mpsc receiver of `SseEvent` into an axum SSE response with keepalive.
pub fn mpsc_to_sse(
    rx: mpsc::Receiver<SseEvent>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = ReceiverStream::new(rx).map(|e| {
        let mut ev = Event::default().data(e.data);
        if !e.event.is_empty() {
            ev = ev.event(e.event);
        }
        Ok(ev)
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
