//! One async stream of every stop request, for an application on Tokio
//! (listener).

use tokio::sync::mpsc;

use super::{on_session_end, StopHold, StopRequest};

/// The stream of stop requests. Dropping it, or calling
/// [`finish`](Self::finish), tells a held Windows stop that the graceful stop
/// is done.
pub struct StopRequests {
    requests: mpsc::UnboundedReceiver<StopRequest>,
    problems: Vec<String>,
    hold: Option<StopHold>,
}

impl StopRequests {
    /// The next request; `None` only if every listener has ended.
    pub async fn recv(&mut self) -> Option<StopRequest> {
        self.requests.recv().await
    }

    /// The requests that could not be listened for, in words. The others
    /// still work; the application decides how to say so.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Say the graceful stop has finished, so a Windows logoff or shutdown
    /// waiting on it can go ahead. Call it as late as possible — after the
    /// last file is flushed — since Windows may end the process straight away.
    pub fn finish(&self) {
        if let Some(hold) = &self.hold {
            hold.finish();
        }
    }
}

/// Start listening for every stop request: Ctrl-C everywhere, SIGTERM on Unix
/// — what `systemctl stop` sends — and Ctrl-Break, console close, logoff and
/// shutdown on Windows. Each listener is registered before this returns, so a
/// request that arrives straight afterwards is not missed. Call it inside a
/// Tokio runtime.
///
/// Tokio's console-close handler does not return, so Windows waits on a
/// console close until the process exits or its own time limit passes.
pub fn listen() -> StopRequests {
    let (tx, requests) = mpsc::unbounded_channel();
    let mut problems = Vec::new();

    let interrupt = tx.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = interrupt.send(StopRequest::Interrupt);
        }
    });

    macro_rules! forward {
        ($request:expr, $listener:expr) => {
            match $listener {
                Ok(mut listener) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if listener.recv().await.is_some() {
                            let _ = tx.send($request);
                        }
                    });
                }
                Err(error) => problems.push(format!("cannot listen for {} ({error})", $request)),
            }
        };
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        forward!(StopRequest::Terminate, signal(SignalKind::terminate()));
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        forward!(StopRequest::Break, windows::ctrl_break());
        forward!(StopRequest::ConsoleClose, windows::ctrl_close());
    }

    let session_tx = tx.clone();
    let hold = match on_session_end(move |request| {
        let _ = session_tx.send(request);
    }) {
        Ok(hold) => Some(hold),
        Err(error) => {
            problems.push(format!("cannot listen for logoff and shutdown ({error})"));
            None
        }
    };

    StopRequests {
        requests,
        problems,
        hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_is_a_stop_request() {
        // `systemctl stop` sends SIGTERM, which must reach the application as
        // a request, not end the process where it stands.
        let mut requests = listen();
        assert!(requests.problems().is_empty(), "{:?}", requests.problems());
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success());
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .expect("the request arrives");
        assert_eq!(request, Some(StopRequest::Terminate));
    }

    #[tokio::test]
    async fn the_stream_registers_without_problems() {
        let requests = listen();
        assert!(requests.problems().is_empty(), "{:?}", requests.problems());
    }
}
