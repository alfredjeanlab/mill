/// A single SSE event with an optional event type and data payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Line-based SSE parser over a reqwest response body.
///
/// Reads chunks from the response and splits into lines, accumulating
/// `event:` and `data:` fields and emitting events on blank lines.
pub struct SseStream {
    response: reqwest::Response,
    buf: String,
    current_event: Option<String>,
    current_data: Vec<String>,
}

impl SseStream {
    pub fn new(response: reqwest::Response) -> Self {
        Self { response, buf: String::new(), current_event: None, current_data: Vec::new() }
    }

    /// Read the next SSE event from the stream.
    /// Returns `None` when the stream ends.
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, std::io::Error> {
        loop {
            // Process any complete lines already in the buffer.
            if let Some(event) = self.process_buffered_lines() {
                return Ok(Some(event));
            }

            // Read the next chunk from the response.
            let chunk = self.response.chunk().await.map_err(std::io::Error::other)?;

            match chunk {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buf.push_str(&text);
                }
                None => {
                    // Stream ended — flush any partial event.
                    if !self.current_data.is_empty() {
                        let event = SseEvent {
                            event: self.current_event.take(),
                            data: self.current_data.join("\n"),
                        };
                        self.current_data.clear();
                        return Ok(Some(event));
                    }
                    return Ok(None);
                }
            }
        }
    }

    /// Process complete lines in the buffer. Returns an event if a blank
    /// line boundary is found, otherwise returns None (need more data).
    fn process_buffered_lines(&mut self) -> Option<SseEvent> {
        loop {
            let newline_pos = self.buf.find('\n')?;
            let line = self.buf[..newline_pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=newline_pos);

            if line.is_empty() {
                // Blank line = event boundary.
                if !self.current_data.is_empty() {
                    let event = SseEvent {
                        event: self.current_event.take(),
                        data: self.current_data.join("\n"),
                    };
                    self.current_data.clear();
                    return Some(event);
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.current_data.push(rest.trim().to_string());
            }
            // Ignore comments (lines starting with ':') and unknown fields.
        }
    }
}
