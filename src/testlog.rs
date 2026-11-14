//! Log capture for tests, without the ordering hazard of per-test subscribers.
//!
//! `tracing` caches whether each callsite is enabled, process-wide. A test that installs its
//! own scoped subscriber can lose a race with another test thread that evaluates the same
//! callsite meanwhile, and its assertion then fails depending on scheduling. Here one global
//! subscriber, enabled at TRACE, is installed once, so every callsite is always enabled; each
//! test then collects only the events its own thread emits into a thread-local buffer.
//! Code under test must therefore log on the test's thread (a current-thread runtime does).

use std::cell::RefCell;
use std::io::Write;
use std::sync::{Arc, Mutex, Once};

thread_local! {
    static BUFFER: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
struct ThreadWriter;

impl Write for ThreadWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        BUFFER.with(|slot| {
            if let Some(buffer) = slot.borrow().as_ref() {
                buffer.lock().expect("log buffer").extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadWriter {
    type Writer = ThreadWriter;

    fn make_writer(&'a self) -> ThreadWriter {
        *self
    }
}

/// Collects this thread's log output from creation until it is dropped.
pub struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    pub fn start() -> Capture {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(ThreadWriter)
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other test installs a global subscriber");
        });
        let buffer = Arc::new(Mutex::new(Vec::new()));
        BUFFER.with(|slot| *slot.borrow_mut() = Some(buffer.clone()));
        Capture(buffer)
    }

    pub fn logs(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        BUFFER.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Run `f` and return what it logged on this thread.
pub fn capture_logs(f: impl FnOnce()) -> String {
    let capture = Capture::start();
    f();
    capture.logs()
}
