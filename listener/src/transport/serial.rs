//! Serial transport (spec §14).
//!
//! Unlike UDP/TCP, a serial port has no async-native API, so its continuous
//! blocking receive loop runs on a **dedicated OS thread** that owns the port
//! handle (ADR-001 / §97.1). That thread hands data to the async runtime over a
//! bounded `tokio::sync::mpsc` — the one edge permitted to stall the reader
//! (§97.2, §99): a full Transport→Pipeline queue backpressures the read loop
//! (a bounded retry loop, so the stall notice fires mid-stall and cancellation
//! is observed), which can cause a UART/driver overrun reported as
//! transport-specific loss (§101).
//!
//! Cancellation is cooperative (§111): the port is opened with a bounded read
//! timeout, so the loop periodically returns from a blocking read to observe the
//! [`CancellationToken`] — shutdown never relies on interrupting an in-progress
//! read. Completion is signalled through a `oneshot`, so awaiting the thread
//! never blocks a runtime worker ([`TransportJoinHandle::Thread`], §138).
//!
//! Opening the port is a bounded blocking operation and runs on `spawn_blocking`
//! (§97.1); it is fallible so resource errors surface at Channel Start (§71).
//! The continuous read loop is generic over a small `BlockingReader` seam so
//! its logic is unit-testable without serial hardware.

use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::{lock_recover, ChannelId, ChunkTime, RuntimeEvent};

use super::{
    DataTransportRunner, ReceivedData, ReceivedPayload, TransportJoinHandle, TransportNotice,
    TransportOutcome,
};

/// Live serial control/status line state (§14.3, §161). Outputs (RTS, DTR) are
/// driven by Listener; inputs (CTS, DSR, DCD, RI) are driven by the device. Output
/// states reflect what Listener set at open or since (serial outputs are not
/// read back), inputs reflect the last poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SerialControlLines {
    pub rts: bool,
    pub dtr: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

/// Authoritative Serial Transport-to-Pipeline backpressure for one channel run.
///
/// Completed totals and the active episode are deliberately separate: a snapshot
/// can show the exact completed history alongside the elapsed time of the current
/// stall without pretending that an unfinished episode has already completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SerialStallStateSnapshot {
    pub(crate) completed_episodes: u64,
    pub(crate) completed_total: Duration,
    pub(crate) completed_max: Duration,
    pub(crate) active_since: Option<Instant>,
}

/// Shared reader-owned Serial stall state.
///
/// The blocking reader updates this cell only at stall boundaries. Runtime
/// snapshots read it directly, so dropping an advisory warning cannot corrupt
/// cumulative totals or leave an episode looking active forever.
#[derive(Clone, Debug, Default)]
pub(crate) struct SerialStallState {
    inner: Arc<Mutex<SerialStallStateSnapshot>>,
}

impl SerialStallState {
    fn lock(&self) -> std::sync::MutexGuard<'_, SerialStallStateSnapshot> {
        lock_recover(&self.inner)
    }

    /// Begin a stall if none is active. Returns whether this call opened it.
    pub(crate) fn begin_at(&self, now: Instant) -> bool {
        let mut state = self.lock();
        if state.active_since.is_some() {
            return false;
        }
        state.active_since = Some(now);
        true
    }

    /// Complete the active stall exactly once. Returns whether one was active.
    pub(crate) fn finish_at(&self, now: Instant) -> bool {
        let mut state = self.lock();
        let Some(started) = state.active_since.take() else {
            return false;
        };
        let elapsed = now.saturating_duration_since(started);
        state.completed_episodes = state.completed_episodes.saturating_add(1);
        state.completed_total = state.completed_total.saturating_add(elapsed);
        state.completed_max = state.completed_max.max(elapsed);
        true
    }

    pub(crate) fn snapshot(&self) -> SerialStallStateSnapshot {
        *self.lock()
    }
}

/// A live control-line command to a running serial Channel (§161).
#[derive(Clone, Copy, Debug)]
pub enum SerialControlCommand {
    SetRts(bool),
    SetDtr(bool),
}

/// The runtime's hooks into a running serial reader's control lines (§161): a
/// command inbox, a shared state cell the reader updates, and the event stream for
/// `ControlLinesChanged` signals. The reader polls inputs and applies commands
/// between bounded reads, so this never interferes with reception (§100).
pub struct SerialControlHooks {
    pub commands: Receiver<SerialControlCommand>,
    pub state: Arc<Mutex<SerialControlLines>>,
    pub events: Sender<RuntimeEvent>,
}

/// Reader-side observation and control hooks kept together so the receive loop's
/// transport inputs stay distinct from its optional runtime integrations.
#[derive(Default)]
struct SerialReceiveHooks {
    notices: Option<Sender<TransportNotice>>,
    control: Option<SerialControlHooks>,
    stall_state: SerialStallState,
}

/// Read buffer size for one blocking read. A serial read returns whatever bytes
/// are available; the chunk boundary is a reception detail, not stream structure
/// (ADR-010 — the stream is never reframed).
const READ_BUFFER: usize = 4096;

/// Default bounded read timeout. Caps how long a blocking read parks before the
/// loop re-checks cancellation (§111).
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// How long the reader must be stalled on the Transport→Pipeline edge before it
/// emits a [`TransportNotice::ReceptionStalled`] (§99, §101; listener ADR-007). A
/// stall this long means the OS/UART receive buffer has had ample time to overrun.
/// Heuristic: momentary backpressure that drains quickly is normal and must not
/// cry loss. The byte count of any overrun is not observable from userland, so the
/// notice carries the stall duration, not a fabricated count (§101).
const STALL_WARNING: Duration = Duration::from_millis(250);

/// Retry cadence while stalled on a full Transport→Pipeline queue. The stall is
/// a poll loop (not a parked `blocking_send`) so the notice can be raised *while*
/// the stall is ongoing — a permanently wedged pipeline must not be silent — and
/// so cancellation still ends the loop (§111). Polling only costs while already
/// stalled, when reception is degraded anyway.
const STALL_POLL: Duration = Duration::from_millis(5);

/// How often the input control lines (CTS/DSR/DCD/RI, §161) are polled. Each poll
/// is four synchronous driver ioctls; doing them before *every* read put four
/// driver round-trips on the hot reception path per chunk — at high chunk rates,
/// far more driver traffic than the data itself. Line changes are human-scale
/// events (a device asserting DTR, a cable unplugged); ~10 Hz shows them as
/// instantly as the GUI can render while costing a bounded ~40 driver calls/s.
/// Pending RTS/DTR **commands** are still applied every pass (operator actions
/// stay immediate), and applying one polls the inputs right away for feedback.
const CONTROL_LINE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// An unopened serial transport description (§14, §74). Call
/// [`open`](Self::open) at Channel Start to acquire the port.
///
/// Parameters use `serialport`'s native enums. The profile schema's richer
/// `Parity`/`StopBits` variants (§80.1: Mark/Space parity, 1.5 stop bits) that
/// `serialport` cannot represent are rejected when config maps to this type.
#[derive(Clone, Debug)]
pub struct SerialTransport {
    channel_id: ChannelId,
    port: String,
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
    rts: Option<bool>,
    dtr: Option<bool>,
    read_timeout: Duration,
}

impl SerialTransport {
    /// A serial transport at 8N1, no flow control — the §82 template defaults.
    pub fn new(channel_id: ChannelId, port: impl Into<String>, baud_rate: u32) -> Self {
        Self {
            channel_id,
            port: port.into(),
            baud_rate,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            rts: None,
            dtr: None,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }

    pub fn with_data_bits(mut self, data_bits: DataBits) -> Self {
        self.data_bits = data_bits;
        self
    }

    pub fn with_parity(mut self, parity: Parity) -> Self {
        self.parity = parity;
        self
    }

    pub fn with_stop_bits(mut self, stop_bits: StopBits) -> Self {
        self.stop_bits = stop_bits;
        self
    }

    pub fn with_flow_control(mut self, flow_control: FlowControl) -> Self {
        self.flow_control = flow_control;
        self
    }

    /// Set the initial RTS line state (§14.2). `None` leaves it OS-default.
    pub fn with_rts(mut self, rts: bool) -> Self {
        self.rts = Some(rts);
        self
    }

    /// Set the initial DTR line state (§14.2).
    pub fn with_dtr(mut self, dtr: bool) -> Self {
        self.dtr = Some(dtr);
        self
    }

    /// The control lines as this transport leaves them at open: the outputs it
    /// sets, and everything else not asserted. A line left at the OS default
    /// cannot be read back, so it is shown as not asserted.
    fn opened_lines(&self) -> SerialControlLines {
        SerialControlLines {
            rts: self.rts.unwrap_or(false),
            dtr: self.dtr.unwrap_or(false),
            ..SerialControlLines::default()
        }
    }

    pub fn with_read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    /// Open the port (§8.2). A bounded blocking op, run on `spawn_blocking`
    /// (§97.1). Fallible so the runtime can take Starting → Faulted (§9, §71).
    pub async fn open(self) -> serialport::Result<OpenSerialTransport> {
        match tokio::task::spawn_blocking(move || self.open_blocking()).await {
            Ok(result) => result,
            // A panicked open task (e.g. a driver-provoked panic inside the
            // serial crate) becomes an open error like any other, so the
            // runtime takes Starting → Faulted instead of poisoning the app —
            // production paths never panic.
            Err(join_err) => Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                format!("serial open task failed: {join_err}"),
            )),
        }
    }

    /// Name the port in an open failure; the OS text is otherwise verbatim.
    ///
    /// `serialport` reports only the OS description, which does not say which
    /// port it was. Why an enumerated port can still fail to open is the UI's
    /// to explain (`serial_port_hint`) — only it knows what is currently
    /// listed, and saying it here too put the same sentence on screen twice.
    fn name_the_port(port: &str, error: serialport::Error) -> serialport::Error {
        serialport::Error::new(
            error.kind(),
            format!("opening serial port {port:?}: {}", error.description),
        )
    }

    fn open_blocking(self) -> serialport::Result<OpenSerialTransport> {
        let lines = self.opened_lines();
        let mut port = serialport::new(&self.port, self.baud_rate)
            .data_bits(self.data_bits)
            .parity(self.parity)
            .stop_bits(self.stop_bits)
            .flow_control(self.flow_control)
            .timeout(self.read_timeout)
            .open()
            .map_err(|e| Self::name_the_port(&self.port, e))?;
        if let Some(rts) = self.rts {
            port.write_request_to_send(rts)?;
        }
        if let Some(dtr) = self.dtr {
            port.write_data_terminal_ready(dtr)?;
        }
        Ok(OpenSerialTransport {
            channel_id: self.channel_id,
            port,
            lines,
            notices: None,
            control: None,
            stall_state: SerialStallState::default(),
        })
    }
}

/// An opened serial port ready to receive. Implements [`DataTransportRunner`].
pub struct OpenSerialTransport {
    channel_id: ChannelId,
    port: Box<dyn SerialPort>,
    /// The control lines as open left them, which seed the shared state cell.
    lines: SerialControlLines,
    /// Optional sink for transport notices (§95, §101). When set, a sustained
    /// reader stall sends `ReceptionStalled`; the pipeline turns it into a retained
    /// warning diagnostic and emits the dedicated `RuntimeEvent::ReceptionStalled`
    /// (listener ADR-007).
    notices: Option<Sender<TransportNotice>>,
    /// Optional live control-line hooks (§161): command inbox + state cell + event
    /// sink. When set, the reader services RTS/DTR commands and polls input lines.
    control: Option<SerialControlHooks>,
    /// Exact per-run Transport-to-Pipeline stall state. The reader owns updates;
    /// runtime snapshots hold a clone and read it directly.
    stall_state: SerialStallState,
}

impl OpenSerialTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Attach the transport-notice channel so a sustained reader stall — the only
    /// edge that can backpressure the reader (§97.1) — is reported (§101, ADR-007).
    /// Optional: without it the reader still stalls rather than drops; it just
    /// stays silent about it. Serial is the only transport that can stall the
    /// reader, so it is the only one given a notice sink.
    pub fn with_notice_sender(mut self, notices: Sender<TransportNotice>) -> Self {
        self.notices = Some(notices);
        self
    }

    /// Attach live control-line hooks (§161): the reader applies RTS/DTR commands
    /// and polls CTS/DSR/DCD/RI between reads, updating the shared state cell and
    /// signalling `ControlLinesChanged`. The cell starts from the lines as open
    /// left them, so an output set at open shows from the first snapshot.
    pub fn with_control(mut self, control: SerialControlHooks) -> Self {
        *lock_recover(&control.state) = self.lines;
        self.control = Some(control);
        self
    }

    /// Clone the authoritative per-run stall-state cell for runtime snapshots.
    pub(crate) fn stall_state(&self) -> SerialStallState {
        self.stall_state.clone()
    }
}

impl DataTransportRunner for OpenSerialTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let (done_tx, done_rx) = oneshot::channel();
        let channel_id = self.channel_id;
        let notices = self.notices;
        let control = self.control;
        let stall_state = self.stall_state;
        let reader = SerialReader { port: self.port };
        let spawned = std::thread::Builder::new()
            .name("serial-rx".to_string())
            .spawn(move || {
                let receive_hooks = SerialReceiveHooks {
                    notices,
                    control,
                    stall_state,
                };
                let outcome = run_blocking_receive_loop(
                    channel_id,
                    reader,
                    out,
                    cancel,
                    STALL_WARNING,
                    receive_hooks,
                );
                // Report the outcome to the async side (never blocks a worker).
                let _ = done_tx.send(outcome);
            });
        if let Err(e) = spawned {
            // Thread spawn can fail on resource exhaustion — plausible on a
            // weeks-long logging host, and never worth a panic (§: no panics
            // in production paths). Report it as a faulted transport: the
            // moved `done_tx` was dropped with the failed closure, so make a
            // fresh pre-completed handle carrying the fault.
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(TransportOutcome::Faulted(format!(
                "failed to spawn the serial receive thread: {e}"
            )));
            return TransportJoinHandle::Thread(rx);
        }
        TransportJoinHandle::Thread(done_rx)
    }
}

/// A blocking, dedicated-thread byte source. `read` blocks up to a bounded
/// timeout and returns `Ok(0)` on timeout (no data yet) so the receive loop can
/// poll cancellation (§111). This seam keeps the loop testable without hardware.
trait BlockingReader: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Read the input/status lines `(CTS, DSR, DCD, RI)` (§14.3). Default: unknown.
    fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
        Ok((false, false, false, false))
    }
    /// Drive the RTS output line (§14.3). Default: no-op.
    fn set_rts(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    /// Drive the DTR output line (§14.3). Default: no-op.
    fn set_dtr(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }
}

/// [`BlockingReader`] backed by a real serial port. Maps the port's `TimedOut`
/// error (raised when the bounded read timeout elapses with no data) to `Ok(0)`.
struct SerialReader {
    port: Box<dyn SerialPort>,
}

impl BlockingReader for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
        Ok((
            self.port.read_clear_to_send().map_err(io::Error::other)?,
            self.port.read_data_set_ready().map_err(io::Error::other)?,
            self.port.read_carrier_detect().map_err(io::Error::other)?,
            self.port.read_ring_indicator().map_err(io::Error::other)?,
        ))
    }

    fn set_rts(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_request_to_send(on)
            .map_err(io::Error::other)
    }

    fn set_dtr(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_data_terminal_ready(on)
            .map_err(io::Error::other)
    }
}

/// The dedicated-thread receive loop (§97.1). Runs until cancelled, the reader
/// reports a fatal error, or the pipeline channel closes.
///
/// On the Transport→Pipeline edge — the only one permitted to backpressure the
/// reader (§99) — the loop *stalls* rather than drops, so it loses nothing in
/// process. A stall longer than `stall_warning` means the OS/UART buffer has had
/// time to overrun, so it sends a `ReceptionStalled` notice through `notices`
/// (§101, ADR-007) **while the stall is still ongoing** — once per episode,
/// carrying the stall duration so far — so even a permanently wedged pipeline is
/// reported. The lost byte count is not observable from userland, so it is not
/// reported. Cancellation is observed during a stall too (§111).
fn run_blocking_receive_loop(
    channel_id: ChannelId,
    mut reader: impl BlockingReader,
    out: Sender<ReceivedData>,
    cancel: CancellationToken,
    stall_warning: Duration,
    mut hooks: SerialReceiveHooks,
) -> TransportOutcome {
    let mut buf = vec![0u8; READ_BUFFER];
    // Live control-line state (§161), tracked across the session. It starts from
    // the shared cell, which holds the outputs set at open.
    let mut lines = hooks
        .control
        .as_ref()
        .map(|ctl| *lock_recover(&ctl.state))
        .unwrap_or_default();
    // First input-line poll happens immediately (initial state), then throttled.
    let mut next_line_poll = Instant::now();
    loop {
        // Cooperative cancellation, observed between bounded reads (§111).
        if cancel.is_cancelled() {
            return TransportOutcome::Cancelled;
        }
        // Live control lines (§161): apply pending RTS/DTR commands every pass and
        // poll the input lines at a bounded cadence between reads, so this never
        // interferes with reception (§100) nor floods the driver with ioctls.
        if let Some(ctl) = hooks.control.as_mut() {
            service_control_lines(
                &mut reader,
                ctl,
                &mut lines,
                channel_id,
                &mut next_line_poll,
            );
        }
        match reader.read(&mut buf) {
            // Timeout / no data: loop back to re-check cancellation.
            Ok(0) => continue,
            Ok(n) => {
                let received_at = ChunkTime::now();
                let data = ReceivedData {
                    channel_id,
                    payload: ReceivedPayload::Bytes(buf[..n].to_vec()),
                    received_at,
                };
                // Fast path: a non-full queue accepts at once and ends any stall
                // episode. On Full we stall — retrying, never dropping (§97.1,
                // §99) — as a poll loop rather than a parked `blocking_send`, so
                // the stall notice can be raised while the stall is *ongoing* (a
                // wedged pipeline must not be silent, ADR-007) and cancellation
                // still ends the loop (§111). A `Closed` queue means the pipeline
                // is gone.
                match out.try_send(data) {
                    Ok(()) => {}
                    Err(TrySendError::Closed(_)) => return TransportOutcome::Completed,
                    Err(TrySendError::Full(data)) => {
                        match retry_stalled_send(
                            channel_id,
                            &out,
                            &cancel,
                            data,
                            stall_warning,
                            &hooks.notices,
                            &hooks.stall_state,
                        ) {
                            StalledSendOutcome::Sent => {}
                            StalledSendOutcome::Cancelled => {
                                return TransportOutcome::Cancelled;
                            }
                            StalledSendOutcome::Closed => return TransportOutcome::Completed,
                        }
                    }
                }
            }
            // A read error ends the loop as a fault (§94). A UART/driver overrun
            // may have lost bytes before this; that loss is reported here (§101).
            Err(e) => return TransportOutcome::Faulted(format!("serial read failed: {e}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StalledSendOutcome {
    Sent,
    Cancelled,
    Closed,
}

/// One open stall episode, completed when this guard drops.
///
/// The reader is the only writer of the shared stall state, and a snapshot
/// derives the live elapsed time from `active_since` — so an episode left open
/// does not merely lose a total, it reports a stall that grows forever. Tying
/// completion to the guard's scope makes every exit from the retry loop close
/// it exactly once, including an unwind out of the reader thread.
struct StallEpisode<'a> {
    state: &'a SerialStallState,
}

impl<'a> StallEpisode<'a> {
    fn begin(state: &'a SerialStallState, at: Instant) -> Self {
        let opened = state.begin_at(at);
        debug_assert!(opened, "serial reader opened a nested stall episode");
        Self { state }
    }
}

impl Drop for StallEpisode<'_> {
    fn drop(&mut self) {
        let completed = self.state.finish_at(Instant::now());
        debug_assert!(completed, "serial reader lost its active stall episode");
    }
}

/// Retry one block after the Transport-to-Pipeline queue first reports Full.
///
/// The state transition is centralized here: the episode becomes active before
/// the first retry and is completed by [`StallEpisode`]'s guard on every exit.
fn retry_stalled_send(
    channel_id: ChannelId,
    out: &Sender<ReceivedData>,
    cancel: &CancellationToken,
    mut pending: ReceivedData,
    stall_warning: Duration,
    notices: &Option<Sender<TransportNotice>>,
    stall_state: &SerialStallState,
) -> StalledSendOutcome {
    let stalled_at = Instant::now();
    let _episode = StallEpisode::begin(stall_state, stalled_at);
    let mut warned = false;
    loop {
        if cancel.is_cancelled() {
            break StalledSendOutcome::Cancelled;
        }
        std::thread::sleep(STALL_POLL);
        if cancel.is_cancelled() {
            break StalledSendOutcome::Cancelled;
        }

        // Test the threshold before the retry. If this retry succeeds just
        // after the threshold, the episode still lasted long enough to
        // warrant the advisory warning.
        let waited = stalled_at.elapsed();
        if !warned && waited >= stall_warning {
            if let Some(notices) = notices {
                let _ = notices.try_send(TransportNotice::ReceptionStalled {
                    channel_id,
                    stalled_for: waited,
                });
            }
            warned = true;
        }

        match out.try_send(pending) {
            Ok(()) => break StalledSendOutcome::Sent,
            Err(TrySendError::Closed(_)) => break StalledSendOutcome::Closed,
            Err(TrySendError::Full(again)) => {
                pending = again;
                // A sustained stall risks a UART/driver overrun upstream of us —
                // transport-specific loss we flag but cannot quantify (§101).
                // The shared state remains authoritative even if this
                // once-per-episode, non-blocking notice attempt is dropped.
            }
        }
    }
}

/// Apply any pending RTS/DTR commands and poll the input lines (§161). On any
/// change, update the shared state cell and signal `ControlLinesChanged` (§137) —
/// the cell is the truth, the event is the lightweight signal (ADR-006). A
/// control-line I/O error is ignored (it does not fault the Channel, §96).
///
/// Commands are drained every call; the four input-line ioctls run only when
/// `next_line_poll` is due ([`CONTROL_LINE_POLL_INTERVAL`]) or a command was just
/// applied — they used to run before every read, which at high chunk rates was
/// more driver traffic than the data itself.
fn service_control_lines(
    reader: &mut impl BlockingReader,
    ctl: &mut SerialControlHooks,
    lines: &mut SerialControlLines,
    channel_id: ChannelId,
    next_line_poll: &mut Instant,
) {
    let mut changed = false;
    while let Ok(cmd) = ctl.commands.try_recv() {
        let applied = match cmd {
            SerialControlCommand::SetRts(on) => reader.set_rts(on).map(|()| lines.rts = on),
            SerialControlCommand::SetDtr(on) => reader.set_dtr(on).map(|()| lines.dtr = on),
        };
        changed |= applied.is_ok();
    }
    // A just-applied command re-polls immediately (fresh feedback on lines a
    // driven RTS/DTR may loop back); otherwise honor the cadence.
    let now = Instant::now();
    if !changed && now < *next_line_poll {
        return;
    }
    *next_line_poll = now + CONTROL_LINE_POLL_INTERVAL;
    if let Ok((cts, dsr, dcd, ri)) = reader.read_inputs() {
        if (cts, dsr, dcd, ri) != (lines.cts, lines.dsr, lines.dcd, lines.ri) {
            lines.cts = cts;
            lines.dsr = dsr;
            lines.dcd = dcd;
            lines.ri = ri;
            changed = true;
        }
    }
    if changed {
        // Recover a poisoned cell rather than skipping the write: dropping a
        // live line change would freeze the panel on stale values (`core::sync`).
        *lock_recover(&ctl.state) = *lines;
        let _ = ctl
            .events
            .try_send(RuntimeEvent::ControlLinesChanged(channel_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use tokio::sync::mpsc;

    async fn wait_for_active_stall(state: &SerialStallState) -> SerialStallStateSnapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = state.snapshot();
                if snapshot.active_since.is_some() {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("serial stall should become active")
    }

    async fn wait_for_completed_stalls(
        state: &SerialStallState,
        episodes: u64,
    ) -> SerialStallStateSnapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = state.snapshot();
                if snapshot.completed_episodes >= episodes && snapshot.active_since.is_none() {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("serial stall should complete")
    }

    #[test]
    fn stall_state_keeps_active_episode_separate_until_completion() {
        let state = SerialStallState::default();
        let started = Instant::now();

        assert!(state.begin_at(started));
        assert!(!state.begin_at(started + Duration::from_millis(1)));
        assert_eq!(
            state.snapshot(),
            SerialStallStateSnapshot {
                completed_episodes: 0,
                completed_total: Duration::ZERO,
                completed_max: Duration::ZERO,
                active_since: Some(started),
            }
        );

        assert!(state.finish_at(started + Duration::from_millis(7)));
        assert!(!state.finish_at(started + Duration::from_millis(9)));
        assert_eq!(
            state.snapshot(),
            SerialStallStateSnapshot {
                completed_episodes: 1,
                completed_total: Duration::from_millis(7),
                completed_max: Duration::from_millis(7),
                active_since: None,
            }
        );
    }

    #[test]
    fn stall_state_accumulates_multiple_including_subthreshold_episodes() {
        let state = SerialStallState::default();
        let started = Instant::now();

        assert!(state.begin_at(started));
        assert!(state.finish_at(started + Duration::from_millis(2)));
        assert!(state.begin_at(started + Duration::from_millis(10)));
        assert!(state.finish_at(started + Duration::from_millis(15)));

        assert_eq!(
            state.snapshot(),
            SerialStallStateSnapshot {
                completed_episodes: 2,
                completed_total: Duration::from_millis(7),
                completed_max: Duration::from_millis(5),
                active_since: None,
            }
        );
    }

    #[test]
    fn stall_state_recovers_authoritative_values_after_mutex_poisoning() {
        let state = SerialStallState::default();
        let started = Instant::now();
        assert!(state.begin_at(started));

        let poisoned = state.clone();
        let _ = catch_unwind(AssertUnwindSafe(move || {
            let _guard = poisoned.inner.lock().unwrap();
            panic!("poison the serial stall state");
        }));

        assert!(state.finish_at(started + Duration::from_millis(3)));
        assert_eq!(state.snapshot().completed_episodes, 1);
        assert_eq!(state.snapshot().completed_total, Duration::from_millis(3));
        assert_eq!(state.snapshot().active_since, None);
    }

    #[test]
    fn an_unwind_mid_stall_still_completes_the_episode() {
        // An episode left open would not just lose a total: a snapshot derives the
        // live elapsed time from `active_since`, so the panel would report a stall
        // growing forever on a reader that is already gone.
        let state = SerialStallState::default();
        let started = Instant::now();

        let panicking = state.clone();
        let unwound = catch_unwind(AssertUnwindSafe(move || {
            let _episode = StallEpisode::begin(&panicking, started);
            panic!("reader thread died mid-stall");
        }));

        assert!(unwound.is_err(), "the panic propagated");
        let snapshot = state.snapshot();
        assert_eq!(snapshot.active_since, None, "no episode is left active");
        assert_eq!(snapshot.completed_episodes, 1, "the episode was completed");
    }

    /// A [`BlockingReader`] that replays a script of reads, then behaves like an
    /// idle port: a brief sleep + `Ok(0)` (timeout) so the loop polls cancel.
    struct ScriptedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ScriptedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
            }
        }
    }

    impl BlockingReader for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.chunks.pop_front() {
                Some(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(n)
                }
                None => {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(0)
                }
            }
        }
    }

    /// A test reader for control-line behavior: never yields data, exposes mutable
    /// input lines, and records the RTS/DTR it was asked to drive.
    #[derive(Clone, Default)]
    struct ControlReader {
        inputs: Arc<Mutex<(bool, bool, bool, bool)>>, // cts, dsr, dcd, ri
        outputs: Arc<Mutex<(bool, bool)>>,            // rts, dtr
    }

    impl BlockingReader for ControlReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(1));
            Ok(0)
        }
        fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
            Ok(*self.inputs.lock().unwrap())
        }
        fn set_rts(&mut self, on: bool) -> io::Result<()> {
            self.outputs.lock().unwrap().0 = on;
            Ok(())
        }
        fn set_dtr(&mut self, on: bool) -> io::Result<()> {
            self.outputs.lock().unwrap().1 = on;
            Ok(())
        }
    }

    #[tokio::test]
    async fn control_lines_apply_commands_and_report_input_changes() {
        // §161: the reader drives RTS/DTR on command and reports input-line changes
        // via the shared cell + a ControlLinesChanged event.
        let reader = ControlReader::default();
        let inputs = reader.inputs.clone();
        let outputs = reader.outputs.clone();

        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let state = Arc::new(Mutex::new(SerialControlLines::default()));
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let (tx, _rx) = mpsc::channel(8);

        let hooks = SerialControlHooks {
            commands: cmd_rx,
            state: state.clone(),
            events: ev_tx,
        };
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                SerialReceiveHooks {
                    control: Some(hooks),
                    ..SerialReceiveHooks::default()
                },
            )
        });

        // Drive RTS high: the output line is set, and the cell + event reflect it.
        cmd_tx
            .send(SerialControlCommand::SetRts(true))
            .await
            .unwrap();
        assert_eq!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::ControlLinesChanged(cid)
        );
        assert!(state.lock().unwrap().rts);
        assert!(outputs.lock().unwrap().0);

        // A device asserts CTS: the next poll detects it and reports the change.
        inputs.lock().unwrap().0 = true;
        loop {
            assert_eq!(
                ev_rx.recv().await.unwrap(),
                RuntimeEvent::ControlLinesChanged(cid)
            );
            if state.lock().unwrap().cts {
                break;
            }
        }

        cancel.cancel();
    }

    #[test]
    fn the_panel_starts_from_the_outputs_set_at_open() {
        let lines = SerialTransport::new(ChannelId::new(), "COM1", 9600)
            .with_rts(true)
            .opened_lines();
        assert!(lines.rts);
        // An unset line keeps the OS default, which cannot be read back.
        assert!(!lines.dtr);
    }

    #[tokio::test]
    async fn an_input_change_keeps_the_outputs_set_at_open() {
        // The cell was seeded with RTS on at open. The loop must start from the
        // cell, so the first input change doesn't report RTS off.
        let reader = ControlReader::default();
        reader.inputs.lock().unwrap().0 = true; // CTS on at the first poll
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let state = Arc::new(Mutex::new(SerialControlLines {
            rts: true,
            ..SerialControlLines::default()
        }));
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let (tx, _rx) = mpsc::channel(8);
        let hooks = SerialControlHooks {
            commands: cmd_rx,
            state: state.clone(),
            events: ev_tx,
        };
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                SerialReceiveHooks {
                    control: Some(hooks),
                    ..SerialReceiveHooks::default()
                },
            )
        });

        assert_eq!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::ControlLinesChanged(cid)
        );
        let lines = *state.lock().unwrap();
        assert!(lines.cts);
        assert!(lines.rts, "RTS was set on at open");
        cancel.cancel();
    }

    #[tokio::test]
    async fn input_line_polling_is_throttled_not_per_read() {
        // The four input-line ioctls used to run before EVERY read — at high chunk
        // rates, more driver round-trips than the data itself. They are now gated
        // to CONTROL_LINE_POLL_INTERVAL. The idle ControlReader turns a read
        // around in ~1 ms, so ~150 ms of loop means ~150 reads: per-read polling
        // would count ~150; the throttle allows the initial poll plus one due
        // refresh (a generous ceiling absorbs scheduler jitter).
        #[derive(Clone, Default)]
        struct CountingReader {
            inner: ControlReader,
            input_polls: Arc<Mutex<usize>>,
        }
        impl BlockingReader for CountingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.inner.read(buf)
            }
            fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
                *self.input_polls.lock().unwrap() += 1;
                self.inner.read_inputs()
            }
            fn set_rts(&mut self, on: bool) -> io::Result<()> {
                self.inner.set_rts(on)
            }
            fn set_dtr(&mut self, on: bool) -> io::Result<()> {
                self.inner.set_dtr(on)
            }
        }

        let reader = CountingReader::default();
        let polls = reader.input_polls.clone();
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, _ev_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(8);
        let hooks = SerialControlHooks {
            commands: cmd_rx,
            state: Arc::new(Mutex::new(SerialControlLines::default())),
            events: ev_tx,
        };
        let loop_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            run_blocking_receive_loop(
                ChannelId::new(),
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                SerialReceiveHooks {
                    control: Some(hooks),
                    ..SerialReceiveHooks::default()
                },
            )
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
        handle.join().unwrap();

        let count = *polls.lock().unwrap();
        assert!(count >= 1, "the initial input-line poll must happen");
        assert!(
            count <= 5,
            "input polling must follow the ~10 Hz cadence, not per-read \
             (got {count} polls in ~150 ms of ~1 ms reads)"
        );
    }

    #[tokio::test]
    async fn receive_loop_emits_chunks_then_stops_on_cancellation() {
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"AB".to_vec(), b"CDE".to_vec()]);
        let (done_tx, done_rx) = oneshot::channel();
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                SerialReceiveHooks::default(),
            );
            let _ = done_tx.send(outcome);
        });

        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"AB");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"CDE");

        // The reader is now idle; cancellation must end the loop within a timeout.
        cancel.cancel();
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn fatal_read_error_yields_a_faulted_outcome() {
        struct FailingReader;
        impl BlockingReader for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("device gone"))
            }
        }

        let (tx, _rx) = mpsc::channel(4);
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                ChannelId::new(),
                FailingReader,
                tx,
                CancellationToken::new(),
                STALL_WARNING,
                SerialReceiveHooks::default(),
            );
            let _ = done_tx.send(outcome);
        });
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Faulted(_)
        ));
    }

    #[tokio::test]
    async fn full_queue_stalls_reader_and_preserves_order() {
        // Capacity 1 forces the bounded retry path to stall after each chunk.
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                SerialReceiveHooks::default(),
            )
        });

        // The reader stalls rather than dropping: all three arrive, in order
        // (§97.1 — the Transport→Pipeline edge stalls instead of losing data).
        // This is the §101 in-process boundary: zero loss inside our queues; any
        // loss would be a UART overrun upstream, outside what we can count.
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"3");

        cancel.cancel();
    }

    #[tokio::test]
    async fn sustained_stall_sends_a_reception_stalled_notice() {
        // §101 / ADR-007: a reader stall longer than the threshold sends a
        // ReceptionStalled notice carrying the stall duration — once per episode,
        // and never by dropping data. A short threshold keeps the test quick.
        let (tx, mut rx) = mpsc::channel(1);
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        // Two chunks: the first fills the cap-1 queue; the second stalls the reader
        // until we drain, after a delay that exceeds the threshold.
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let loop_cancel = cancel.clone();
        let threshold = Duration::from_millis(20);
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                SerialReceiveHooks {
                    notices: Some(notice_tx),
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            )
        });

        // Confirm the stall before waiting for its advisory threshold. Keeping
        // the queue full until the notice arrives avoids scheduler-dependent
        // sleeps and proves the notice is raised while the episode is active.
        wait_for_active_stall(&stall_state).await;
        let notice = tokio::time::timeout(Duration::from_secs(5), notice_rx.recv())
            .await
            .expect("sustained stall should raise its advisory notice")
            .expect("notice sender should remain open");
        match notice {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                assert_eq!(channel_id, cid);
                assert!(stalled_for >= threshold);
            }
            other => panic!("expected ReceptionStalled, got {other:?}"),
        }

        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");

        // Exact completion comes from the shared state, independently of
        // notice delivery.
        let completed = wait_for_completed_stalls(&stall_state, 1).await;
        assert_eq!(completed.completed_episodes, 1);
        assert!(completed.completed_total >= threshold);
        assert_eq!(completed.completed_max, completed.completed_total);
        assert!(notice_rx.try_recv().is_err());

        cancel.cancel();
    }

    #[tokio::test]
    async fn a_wedged_pipeline_raises_the_notice_while_still_stalled_and_can_cancel() {
        // ADR-007 tier 2: the notice must arrive while the stall is ONGOING — a
        // permanently wedged pipeline (never drained) must not be silent. And
        // cancellation must end the stalled reader (§111) even though the queue
        // never drains.
        let (tx, rx) = mpsc::channel(1); // held open, never drained
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        // Chunk 1 fills the cap-1 queue; chunk 2 stalls the reader forever.
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let threshold = Duration::from_millis(20);
        let (done_tx, done_rx) = oneshot::channel();
        let loop_cancel = cancel.clone();
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                SerialReceiveHooks {
                    notices: Some(notice_tx),
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            );
            let _ = done_tx.send(outcome);
        });

        // Without draining anything, the notice arrives mid-stall.
        let notice = tokio::time::timeout(Duration::from_secs(5), notice_rx.recv())
            .await
            .expect("the notice must arrive while the stall is ongoing")
            .unwrap();
        match notice {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                assert_eq!(channel_id, cid);
                assert!(stalled_for >= threshold);
            }
            other => panic!("expected ReceptionStalled, got {other:?}"),
        }
        let active = wait_for_active_stall(&stall_state).await;
        assert_eq!(active.completed_episodes, 0);

        // Cancel while still stalled: the loop ends as Cancelled, not hung.
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("cancellation must end a stalled reader")
            .unwrap();
        assert!(matches!(outcome, TransportOutcome::Cancelled));
        let completed = wait_for_completed_stalls(&stall_state, 1).await;
        assert_eq!(completed.completed_episodes, 1);
        assert!(completed.completed_total >= threshold);
        assert!(notice_rx.try_recv().is_err());
        drop(rx);
    }

    #[tokio::test]
    async fn closing_pipeline_while_stalled_finalizes_the_episode() {
        let (tx, rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                cid,
                reader,
                tx,
                cancel,
                STALL_WARNING,
                SerialReceiveHooks {
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            );
            let _ = done_tx.send(outcome);
        });

        wait_for_active_stall(&stall_state).await;
        drop(rx);
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Completed
        ));
        assert_eq!(
            wait_for_completed_stalls(&stall_state, 1)
                .await
                .completed_episodes,
            1
        );
    }

    #[tokio::test]
    async fn dropped_stall_warning_does_not_affect_authoritative_totals() {
        let (tx, mut rx) = mpsc::channel(1);
        let (notice_tx, mut notice_rx) = mpsc::channel(1);
        let cid = ChannelId::new();
        notice_tx
            .try_send(TransportNotice::UdpKernelDrops {
                channel_id: cid,
                dropped: Some(0),
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                Duration::from_millis(10),
                SerialReceiveHooks {
                    notices: Some(notice_tx),
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            )
        });

        wait_for_active_stall(&stall_state).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");
        let completed = wait_for_completed_stalls(&stall_state, 1).await;
        assert_eq!(completed.completed_episodes, 1);
        assert!(completed.completed_total >= Duration::from_millis(10));
        assert!(matches!(
            notice_rx.try_recv(),
            Ok(TransportNotice::UdpKernelDrops { .. })
        ));
        assert!(notice_rx.try_recv().is_err());
        cancel.cancel();
    }

    #[tokio::test]
    async fn read_error_after_completed_stall_preserves_exact_totals() {
        struct TwoChunksThenError(u8);
        impl BlockingReader for TwoChunksThenError {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 < 2 {
                    buf[0] = b'1' + self.0;
                    self.0 += 1;
                    Ok(1)
                } else {
                    Err(io::Error::other("device gone"))
                }
            }
        }

        let (tx, mut rx) = mpsc::channel(1);
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                ChannelId::new(),
                TwoChunksThenError(0),
                tx,
                CancellationToken::new(),
                STALL_WARNING,
                SerialReceiveHooks {
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            );
            let _ = done_tx.send(outcome);
        });

        wait_for_active_stall(&stall_state).await;
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Faulted(_)
        ));
        let completed = wait_for_completed_stalls(&stall_state, 1).await;
        assert_eq!(completed.completed_episodes, 1);
        assert!(completed.completed_total > Duration::ZERO);
    }

    #[tokio::test]
    async fn momentary_backpressure_updates_totals_without_a_warning() {
        // A queue that drains promptly is normal backpressure, not possible loss:
        // account for the wait but emit no ReceptionStalled warning.
        let (tx, mut rx) = mpsc::channel(1);
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        let loop_cancel = cancel.clone();
        // A high threshold the brisk draining below never crosses.
        let threshold = Duration::from_secs(10);
        let stall_state = SerialStallState::default();
        let reader_stall_state = stall_state.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                SerialReceiveHooks {
                    notices: Some(notice_tx),
                    stall_state: reader_stall_state,
                    ..SerialReceiveHooks::default()
                },
            )
        });

        // Synchronize on the reader-owned state so this test cannot
        // accidentally drain fast enough to avoid backpressure altogether.
        wait_for_active_stall(&stall_state).await;
        for expected in [b"1", b"2", b"3"] {
            assert_eq!(rx.recv().await.unwrap().payload.bytes(), expected);
        }
        let completed = wait_for_completed_stalls(&stall_state, 1).await;
        assert!(completed.completed_episodes >= 1);
        assert!(completed.completed_total > Duration::ZERO);
        cancel.cancel();

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            notice_rx.try_recv().is_err(),
            "brief backpressure must not raise a warning"
        );
    }

    #[test]
    fn first_successful_retry_at_threshold_still_attempts_warning() {
        let (tx, mut rx) = mpsc::channel(1);
        let channel_id = ChannelId::new();
        tx.try_send(ReceivedData {
            channel_id,
            received_at: ChunkTime::now(),
            payload: ReceivedPayload::Bytes(b"first".to_vec()),
        })
        .unwrap();
        let pending = match tx.try_send(ReceivedData {
            channel_id,
            received_at: ChunkTime::now(),
            payload: ReceivedPayload::Bytes(b"pending".to_vec()),
        }) {
            Err(TrySendError::Full(pending)) => pending,
            other => panic!("capacity-one queue should initially be full: {other:?}"),
        };

        // The consumer frees capacity before the first retry. With a zero
        // threshold, that successful retry is itself the threshold edge.
        assert_eq!(rx.try_recv().unwrap().payload.bytes(), b"first");
        let (notice_tx, mut notice_rx) = mpsc::channel(1);
        let stall_state = SerialStallState::default();
        assert_eq!(
            retry_stalled_send(
                channel_id,
                &tx,
                &CancellationToken::new(),
                pending,
                Duration::ZERO,
                &Some(notice_tx),
                &stall_state,
            ),
            StalledSendOutcome::Sent
        );

        assert!(matches!(
            notice_rx.try_recv(),
            Ok(TransportNotice::ReceptionStalled { .. })
        ));
        assert_eq!(rx.try_recv().unwrap().payload.bytes(), b"pending");
        let snapshot = stall_state.snapshot();
        assert_eq!(snapshot.completed_episodes, 1);
        assert!(snapshot.active_since.is_none());
    }
}
