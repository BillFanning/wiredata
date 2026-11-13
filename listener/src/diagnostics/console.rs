//! The console, written without waiting on it (§114).
//!
//! A console can stop taking output: selecting text pauses a classic Windows
//! console window, and a pipe stalls when its reader does. A thread that writes
//! to it directly then waits with it, and the threads that would write are the
//! ones receiving, recording and reconnecting.
//!
//! So everything Listener prints while Channels run goes through a bounded
//! queue to a worker thread, one per stream. When a queue is full the line is
//! dropped and counted, and nothing waits. Console output is therefore
//! best-effort; the event log (§118) is the record.

use std::fmt::Display;
use std::io::Write;

use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};

/// Lines each stream holds for a console that is slow or paused. Past this,
/// lines are dropped and counted.
pub const CONSOLE_QUEUE_LINES: usize = 4_096;

/// Standard output and standard error, each behind its own bounded queue.
/// Cheap to clone; every clone feeds the same two queues, so lines on one
/// stream come out in the order they were written.
#[derive(Clone)]
pub struct Console {
    out: NonBlocking,
    err: NonBlocking,
}

impl Console {
    /// Write one line to standard output. Never waits; a full queue drops it.
    pub fn out(&self, line: impl Display) {
        Self::write(&self.out, line);
    }

    /// Write one line to standard error. Never waits; a full queue drops it.
    pub fn err(&self, line: impl Display) {
        Self::write(&self.err, line);
    }

    /// One queue entry per line, so lines from different threads never
    /// interleave within a line.
    fn write(stream: &NonBlocking, line: impl Display) {
        let _ = stream.clone().write_all(format!("{line}\n").as_bytes());
    }

    /// How many lines were dropped because a queue was full.
    pub fn lines_not_shown(&self) -> u64 {
        let dropped =
            self.out.error_counter().dropped_lines() + self.err.error_counter().dropped_lines();
        dropped as u64
    }

    /// The standard-output queue as a `tracing` writer, so log lines keep
    /// their order with the lines written through [`out`](Self::out).
    pub(crate) fn log_writer(&self) -> NonBlocking {
        self.out.clone()
    }

    /// A console that shows nothing, for tests of code that prints.
    #[cfg(test)]
    pub(crate) fn discard() -> Self {
        ConsoleGuard::over(std::io::sink(), std::io::sink()).console()
    }
}

/// Keeps the console's two worker threads running. Dropping it writes out what
/// is queued, waiting about a second at most for each stream, and ends them;
/// lines written afterwards are dropped.
#[must_use = "dropping the guard stops console output; hold it until the process exits"]
pub struct ConsoleGuard {
    console: Console,
    _out: WorkerGuard,
    _err: WorkerGuard,
}

impl ConsoleGuard {
    /// The process's standard output and standard error.
    pub fn open() -> Self {
        Self::over(std::io::stdout(), std::io::stderr())
    }

    fn over(out: impl Write + Send + 'static, err: impl Write + Send + 'static) -> Self {
        let queue = |stream| {
            NonBlockingBuilder::default()
                .lossy(true)
                .buffered_lines_limit(CONSOLE_QUEUE_LINES)
                .thread_name("listener-console")
                .finish(stream)
        };
        let (out, out_guard) = queue(Box::new(out) as Box<dyn Write + Send>);
        let (err, err_guard) = queue(Box::new(err) as Box<dyn Write + Send>);
        Self {
            console: Console { out, err },
            _out: out_guard,
            _err: err_guard,
        }
    }

    /// A handle for writing lines.
    pub fn console(&self) -> Console {
        self.console.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Collects what is written, for reading back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// A console that takes nothing until it is released: what a paused
    /// console window is to the thread writing to it.
    struct Paused(mpsc::Receiver<()>);

    impl Write for Paused {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_console_that_takes_no_output_does_not_hold_up_the_writer() {
        let (release, paused) = mpsc::channel();
        let guard = ConsoleGuard::over(Paused(paused), std::io::sink());
        let console = guard.console();

        // Far more than the queue holds. Written directly, the first line
        // would wait for the console and this test would never finish.
        let lines = CONSOLE_QUEUE_LINES * 3;
        let started = Instant::now();
        for n in 0..lines {
            console.out(format_args!("line {n}"));
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "writing waited on the console"
        );
        // The worker holds one line and the queue the rest it has room for.
        let dropped = console.lines_not_shown();
        assert!(
            dropped >= (lines - CONSOLE_QUEUE_LINES - 1) as u64,
            "{dropped} of {lines} dropped"
        );
        assert!(dropped < lines as u64, "some lines were queued");

        // Let the worker end, so the guard has nothing to wait for.
        drop(release);
    }

    #[test]
    fn lines_come_out_whole_and_in_the_order_written() {
        let (out, err) = (Shared::default(), Shared::default());
        let guard = ConsoleGuard::over(out.clone(), err.clone());
        let console = guard.console();
        console.out("started \"GPS\"");
        console.err("WARNING: [GPS] is down");
        console.clone().out("listening");
        // Dropping the guard writes out what is queued.
        drop(guard);
        assert_eq!(out.text(), "started \"GPS\"\nlistening\n");
        assert_eq!(err.text(), "WARNING: [GPS] is down\n");
        assert_eq!(console.lines_not_shown(), 0);
    }

    #[test]
    fn log_lines_share_the_queue_with_printed_lines() {
        let out = Shared::default();
        let guard = ConsoleGuard::over(out.clone(), std::io::sink());
        let console = guard.console();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(console.log_writer())
            .finish();
        console.out("before");
        crate::diagnostics::test_support::with_subscriber(subscriber, || {
            tracing::info!("logged in between");
        });
        console.out("after");
        drop(guard);
        let text = out.text();
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {text:?}"))
        };
        assert!(at("before") < at("logged in between"));
        assert!(at("logged in between") < at("after"));
    }
}
