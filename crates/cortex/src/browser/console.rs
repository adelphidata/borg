use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

/// A bounded ring buffer that evicts the oldest entry when capacity is reached.
#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    buf: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    /// Create a new ring buffer with the given maximum capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    /// Append an item, evicting the oldest if at capacity.
    pub fn push(&mut self, item: T) {
        if self.capacity == 0 {
            return;
        }
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    /// Returns the number of items currently in the buffer.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns true if the buffer contains no items.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the maximum number of items this buffer can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drain all entries and return them, clearing the buffer.
    pub fn drain_all(&mut self) -> Vec<T> {
        self.buf.drain(..).collect()
    }

    /// Return a snapshot of all entries without clearing.
    pub fn snapshot(&self) -> Vec<&T> {
        self.buf.iter().collect()
    }
}

/// A captured console API call (console.log, console.warn, etc.).
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    /// Log level (e.g. "log", "warn", "error").
    pub level: String,
    /// The logged message text.
    pub text: String,
    /// CDP timestamp when the entry was captured.
    pub timestamp: f64,
}

impl fmt::Display for ConsoleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.level, self.text)
    }
}

/// A captured unhandled page error / exception.
#[derive(Debug, Clone)]
pub struct PageError {
    /// Error or exception description.
    pub message: String,
    /// CDP timestamp when the error occurred.
    pub timestamp: f64,
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[error] {}", self.message)
    }
}

/// A captured network request with optional response status.
#[derive(Debug, Clone)]
pub struct NetworkEntry {
    /// Request URL.
    pub url: String,
    /// HTTP method (GET, POST, etc.).
    pub method: String,
    /// Response status code, or None if still pending.
    pub status: Option<u16>,
    /// CDP timestamp when the request was initiated.
    pub timestamp: f64,
}

impl fmt::Display for NetworkEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} {} → {}", self.method, self.url, s),
            None => write!(f, "{} {} → pending", self.method, self.url),
        }
    }
}

/// Thread-safe collection of event buffers shared with background drain tasks.
#[derive(Clone)]
pub struct EventBuffers {
    /// Buffer for console.log/warn/error entries.
    pub console: Arc<Mutex<RingBuffer<ConsoleEntry>>>,
    /// Buffer for unhandled page exceptions.
    pub errors: Arc<Mutex<RingBuffer<PageError>>>,
    /// Buffer for captured network requests.
    pub network: Arc<Mutex<RingBuffer<NetworkEntry>>>,
}

impl EventBuffers {
    /// Create new event buffers with the given capacities.
    pub fn new(console_cap: usize, error_cap: usize, network_cap: usize) -> Self {
        Self {
            console: Arc::new(Mutex::new(RingBuffer::new(console_cap))),
            errors: Arc::new(Mutex::new(RingBuffer::new(error_cap))),
            network: Arc::new(Mutex::new(RingBuffer::new(network_cap))),
        }
    }

    /// Format all captured console logs and errors for display.
    pub fn format_console_output(&self) -> String {
        let mut lines = Vec::new();

        if let Ok(console) = self.console.lock() {
            for entry in console.snapshot() {
                lines.push(entry.to_string());
            }
        }

        if let Ok(errors) = self.errors.lock() {
            for entry in errors.snapshot() {
                lines.push(entry.to_string());
            }
        }

        if lines.is_empty() {
            "No console output captured.".to_string()
        } else {
            lines.join("\n")
        }
    }
}

/// Spawn background tasks that drain CDP event streams into the shared buffers.
/// Must be called with `await` since it sets up event listeners before spawning.
/// Returns `JoinHandle`s that can be aborted on session close.
pub async fn spawn_event_listeners(
    page: &chromiumoxide::Page,
    buffers: &EventBuffers,
) -> Vec<tokio::task::JoinHandle<()>> {
    use chromiumoxide::cdp::js_protocol::runtime::{EventConsoleApiCalled, EventExceptionThrown};
    use futures::StreamExt;

    let mut handles = Vec::new();

    // Await the event listener setup while we have `&page`,
    // then move the owned stream into the spawned task.
    if let Ok(mut console_stream) = page.event_listener::<EventConsoleApiCalled>().await {
        let buf = buffers.console.clone();
        handles.push(tokio::spawn(async move {
            while let Some(event) = console_stream.next().await {
                let text = event
                    .args
                    .iter()
                    .filter_map(|a| {
                        a.value
                            .as_ref()
                            .map(|v| v.to_string().trim_matches('"').to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let entry = ConsoleEntry {
                    level: format!("{:?}", event.r#type).to_lowercase(),
                    text,
                    timestamp: *event.timestamp.inner(),
                };
                if let Ok(mut guard) = buf.lock() {
                    guard.push(entry);
                }
            }
        }));
    }

    if let Ok(mut exception_stream) = page.event_listener::<EventExceptionThrown>().await {
        let buf = buffers.errors.clone();
        handles.push(tokio::spawn(async move {
            while let Some(event) = exception_stream.next().await {
                let message = event
                    .exception_details
                    .exception
                    .as_ref()
                    .and_then(|e| e.description.clone())
                    .unwrap_or_else(|| event.exception_details.text.clone());
                let entry = PageError {
                    message,
                    timestamp: *event.timestamp.inner(),
                };
                if let Ok(mut guard) = buf.lock() {
                    guard.push(entry);
                }
            }
        }));
    }

    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_basic_operations() {
        let mut buf = RingBuffer::new(3);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.capacity(), 3);

        buf.push(1);
        buf.push(2);
        buf.push(3);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut buf = RingBuffer::new(3);
        buf.push(1);
        buf.push(2);
        buf.push(3);
        buf.push(4); // evicts 1
        assert_eq!(buf.len(), 3);

        let items = buf.drain_all();
        assert_eq!(items, vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_evicts_many() {
        let mut buf = RingBuffer::new(5);
        for i in 0..100 {
            buf.push(i);
        }
        assert_eq!(buf.len(), 5);
        let items = buf.drain_all();
        assert_eq!(items, vec![95, 96, 97, 98, 99]);
    }

    #[test]
    fn ring_buffer_drain_clears() {
        let mut buf = RingBuffer::new(10);
        buf.push(1);
        buf.push(2);
        let items = buf.drain_all();
        assert_eq!(items, vec![1, 2]);
        assert!(buf.is_empty());
    }

    #[test]
    fn ring_buffer_snapshot_does_not_clear() {
        let mut buf = RingBuffer::new(10);
        buf.push(1);
        buf.push(2);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn ring_buffer_zero_capacity() {
        let mut buf = RingBuffer::new(0);
        buf.push(1); // should not panic, just evict immediately
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn console_entry_display() {
        let entry = ConsoleEntry {
            level: "log".to_string(),
            text: "hello world".to_string(),
            timestamp: 1234.0,
        };
        assert_eq!(entry.to_string(), "[log] hello world");
    }

    #[test]
    fn page_error_display() {
        let entry = PageError {
            message: "ReferenceError: x is not defined".to_string(),
            timestamp: 1234.0,
        };
        assert_eq!(
            entry.to_string(),
            "[error] ReferenceError: x is not defined"
        );
    }

    #[test]
    fn network_entry_display_with_status() {
        let entry = NetworkEntry {
            url: "https://example.com/api".to_string(),
            method: "GET".to_string(),
            status: Some(200),
            timestamp: 1234.0,
        };
        assert_eq!(entry.to_string(), "GET https://example.com/api → 200");
    }

    #[test]
    fn network_entry_display_pending() {
        let entry = NetworkEntry {
            url: "https://example.com/api".to_string(),
            method: "POST".to_string(),
            status: None,
            timestamp: 1234.0,
        };
        assert_eq!(entry.to_string(), "POST https://example.com/api → pending");
    }

    #[test]
    fn event_buffers_format_empty() {
        let bufs = EventBuffers::new(10, 10, 10);
        assert_eq!(bufs.format_console_output(), "No console output captured.");
    }

    #[test]
    fn event_buffers_format_with_entries() {
        let bufs = EventBuffers::new(10, 10, 10);
        bufs.console.lock().unwrap().push(ConsoleEntry {
            level: "log".to_string(),
            text: "test".to_string(),
            timestamp: 0.0,
        });
        bufs.errors.lock().unwrap().push(PageError {
            message: "oops".to_string(),
            timestamp: 0.0,
        });
        let output = bufs.format_console_output();
        assert!(output.contains("[log] test"));
        assert!(output.contains("[error] oops"));
    }
}
