// Modified for Distill by Samuel Fajreldines, 2026.
//! Frame drawing with cursor blink preservation.
//!
//! # Problem
//!
//! Ratatui's [`Terminal::draw()`] (internally `try_draw()`) unconditionally sends cursor escape sequences on every frame:
//!
//! - If `frame.set_cursor_position()` was called: `Show` and `MoveTo` every frame
//! - If not called: `Hide` every frame
//!
//! Both reset the terminal's cursor blink timer (`Show` restarts the blink cycle, `MoveTo` resets the blink phase).
//! At 30fps, the 500ms blink interval never completes, so the cursor appears solid.
//!
//! # Solution
//!
//! We bypass `try_draw()` and use ratatui's lower-level API directly:
//!
//! ```text
//! terminal.autoresize()     — handle terminal size changes
//! terminal.get_frame()      — get a fresh buffer to render into
//! terminal.flush()          — diff old/new buffers, write only changed cells
//! terminal.swap_buffers()   — prepare for next frame
//! ```
//!
//! Cursor is managed entirely by [`CursorState`] with de-duplication:
//!
//! - **No cell changes, same position**: zero cursor commands, so blink is preserved
//! - **Cells changed, same position**: `MoveTo` to fix the cursor after cell writes
//! - **Position changed**: `MoveTo` (blink resets; expected, the user just typed)
//! - **Visibility transition**: `Show`/`Hide` (only on actual transition)
//! - **Idle (no draw calls)**: nothing sent, so blink runs undisturbed
//!
//! The "no cell changes" case is detectable because [`distill_ratatui_inline::Terminal`]'s `flush()` returns whether any cells were written.
//! When animated entries are off-screen, the buffer diff is empty and we skip all cursor commands.
//!
//! # Synchronized output
//!
//! Each frame is wrapped in `BeginSynchronizedUpdate` / `EndSynchronizedUpdate` so the terminal presents the cell diff, images, and cursor moves atomically.
//! The wrapper is omitted when tmux is the immediate terminal (see [`crate::terminal::should_emit_synchronized_output`]): tmux repaints the whole pane when a block closes and already synchronizes its own output toward the outer terminal.
use crate::terminal::{TerminalContext, should_emit_synchronized_output};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{QueueableCommand, cursor};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};
use distill_ratatui_inline::LinkSpan;
/// Defined here (beside [`TermWriter`]) so the `render` module does not depend on `app`.
/// Re-exported from `app` as `crate::app::PagerTerminal` for existing call sites.
pub type PagerTerminal = distill_ratatui_inline::Terminal<CrosstermBackend<TermWriter>>;
#[derive(Debug)]
pub enum WriterEvent {
    Written(u64),
    Failed(std::io::Error),
}
/// Outcome of a bounded writer drain attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterDrain {
    Drained,
    TimedOut,
}
/// Sequence is reserved before send, so an accepted frame blocks the child-handoff drain before the writer sees it. No queued frame lands after the child takes the tty.
#[derive(Clone, Debug)]
pub struct WriterSync {
    queued: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    writer_active: Arc<AtomicBool>,
    /// The one thread allowed to produce payloads, latched on first send (see [`send_payload`]).
    producer: Arc<OnceLock<ThreadId>>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<WriterEvent>>,
}
impl Default for WriterSync {
    fn default() -> Self {
        Self::new()
    }
}
impl WriterSync {
    pub fn new() -> Self {
        Self {
            queued: Arc::new(AtomicU64::new(0)),
            written: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            writer_active: Arc::new(AtomicBool::new(false)),
            producer: Arc::new(OnceLock::new()),
            event_tx: None,
        }
    }
    fn with_event_sender(event_tx: tokio::sync::mpsc::UnboundedSender<WriterEvent>) -> Self {
        Self {
            event_tx: Some(event_tx),
            ..Self::new()
        }
    }
    #[cfg(test)]
    fn new_for_test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<WriterEvent>) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::with_event_sender(event_tx), event_rx)
    }
    fn reserve_sequence(&self) -> u64 {
        self.queued.fetch_add(1, Ordering::Release) + 1
    }
    fn mark_written(&self, sequence: u64) {
        self.written.fetch_max(sequence, Ordering::AcqRel);
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.send(WriterEvent::Written(sequence));
        }
    }
    fn mark_failed(&self, error: std::io::Error) {
        if self
            .failed
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.send(WriterEvent::Failed(error));
        }
    }
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn is_drained(&self) -> bool {
        !self.failed() && self.written() >= self.queued()
    }
    /// Block until the writer flushes every accepted payload, output fails, or the deadline passes.
    pub fn wait_drained(&self, timeout: Duration) -> std::io::Result<WriterDrain> {
        let deadline = Instant::now() + timeout;
        while !self.is_drained() {
            if self.failed() {
                return Err(std::io::Error::other("terminal output failed"));
            }
            if Instant::now() >= deadline {
                return Ok(WriterDrain::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(WriterDrain::Drained)
    }
}
/// Buffers a frame's escape sequences and hands them to a writer thread for the blocking write to stderr / the pty fd.
/// If the terminal emulator is slow to read, only the writer thread stalls; the event loop keeps processing timers, events, and ACP messages.
pub struct WriterPayload {
    pub(crate) sequence: u64,
    pub(crate) data: Vec<u8>,
}
impl WriterPayload {
    /// The raw bytes this payload writes to the tty.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}
pub type WriterSender = mpsc::Sender<WriterPayload>;
fn writer_exited_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "terminal writer thread exited",
    )
}
/// Send failure marks shared sync failed; the event loop surfaces it as fatal.
/// One producer only: interleaved reserve+send would put byte-order-sensitive escapes on the tty out of order.
fn send_payload(tx: &WriterSender, sync: &WriterSync, data: Vec<u8>) -> std::io::Result<()> {
    let current = std::thread::current().id();
    let producer = *sync.producer.get_or_init(|| current);
    debug_assert_eq!(
        producer, current,
        "writer payloads must all come from the event-loop thread; a second producer thread \
         can reorder terminal output (see EscapeWriter)"
    );
    let sequence = sync.reserve_sequence();
    if tx.send(WriterPayload { sequence, data }).is_err() {
        sync.mark_failed(writer_exited_error());
        return Err(writer_exited_error());
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterAlreadyActive;
impl std::fmt::Display for WriterAlreadyActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterSync already owns a live TermWriter")
    }
}
impl std::error::Error for WriterAlreadyActive {}
pub struct TermWriter {
    buf: Vec<u8>,
    tx: WriterSender,
    sync: WriterSync,
}
impl TermWriter {
    pub fn new(tx: WriterSender, sync: WriterSync) -> Result<Self, WriterAlreadyActive> {
        sync.writer_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WriterAlreadyActive)?;
        Ok(Self {
            buf: Vec::with_capacity(32 * 1024),
            tx,
            sync,
        })
    }
    /// Drop the current frame's buffered bytes without sending them.
    pub fn discard(&mut self) {
        self.buf.clear();
    }
    /// Shared writer progress used by the suspend path to [`WriterSync::wait_drained`] before a child takes the tty.
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
    /// A cloneable, non-blocking handle onto this writer's queue for
    /// out-of-band escapes (see [`EscapeWriter`]).
    pub fn escape_writer(&self) -> EscapeWriter {
        EscapeWriter::new(self.tx.clone(), self.sync.clone())
    }
}
impl Write for TermWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        send_payload(&self.tx, &self.sync, std::mem::take(&mut self.buf))
    }
}
impl Drop for TermWriter {
    fn drop(&mut self) {
        let _ = self.flush();
        self.sync.writer_active.store(false, Ordering::Release);
    }
}
/// Enqueue: an inline stderr lock from the event loop deadlocks when the terminal stops reading the pty.
/// Event-loop thread only. Drop every clone before [`WriterThread::join_within`] or the writer never sees the channel close.
#[derive(Clone)]
pub struct EscapeWriter {
    tx: WriterSender,
    sync: WriterSync,
}
impl EscapeWriter {
    pub fn new(tx: WriterSender, sync: WriterSync) -> Self {
        Self { tx, sync }
    }
    /// A writer with no writer thread behind it: sends fail against a private,
    /// receiver-less channel and are dropped. For the headless leader (no tty) and tests;
    /// any TUI view must get the live handle from [`TermWriter::escape_writer`] instead.
    pub fn disconnected() -> Self {
        let (tx, _rx) = mpsc::channel::<WriterPayload>();
        Self {
            tx,
            sync: WriterSync::new(),
        }
    }
    /// Never blocks. Covered by `wait_drained`. Send failure is recorded on the shared sync, not returned.
    pub fn emit(&self, bytes: impl Into<Vec<u8>>) {
        let data: Vec<u8> = bytes.into();
        if data.is_empty() {
            return;
        }
        let _ = send_payload(&self.tx, &self.sync, data);
    }
    /// Winapi-only commands run synchronously: a console API call, not a tty write, so they neither block on the pty nor take the stderr lock.
    pub fn emit_command(&self, command: impl crossterm::Command) {
        #[cfg(windows)]
        if !command.is_ansi_code_supported() {
            let _ = command.execute_winapi();
            return;
        }
        let mut ansi = String::new();
        if command.write_ansi(&mut ansi).is_ok() {
            self.emit(ansi);
        }
    }
}
/// Outcome of [`WriterThread::join_within`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterJoin {
    /// The thread drained its queue and exited.
    Joined,
    /// The thread was still running at the deadline (tty blocked, or a sender still alive)
    /// and has been detached.
    TimedOut,
}
/// Joining ensures all queued frames have been written to the terminal before teardown (e.g. `LeaveAlternateScreen`).
pub struct WriterThread {
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    sync: WriterSync,
}
impl WriterThread {
    /// Every sender, including [`EscapeWriter`] clones, must be dropped first or this can only time out.
    /// A timeout also covers a terminal that stopped reading; the thread is detached and teardown proceeds.
    pub fn join_within(mut self, grace: Duration) -> std::io::Result<WriterJoin> {
        let Some(handle) = self.handle.take() else {
            return Ok(WriterJoin::Joined);
        };
        let deadline = Instant::now() + grace;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    grace_ms = grace.as_millis() as u64,
                    queued = self.sync.queued(),
                    written = self.sync.written(),
                    "term-writer thread still running at teardown; detaching"
                );
                drop(handle);
                return Ok(WriterJoin::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        match handle.join() {
            Ok(result) => result.map(|()| WriterJoin::Joined),
            Err(_) => Err(std::io::Error::other("terminal writer thread panicked")),
        }
    }
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
    #[cfg(test)]
    fn for_test(handle: std::thread::JoinHandle<std::io::Result<()>>, sync: WriterSync) -> Self {
        Self {
            handle: Some(handle),
            sync,
        }
    }
}
impl Drop for WriterThread {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn write_payload(
    writer: &mut impl Write,
    payload: &WriterPayload,
    sync: &WriterSync,
) -> std::io::Result<()> {
    match writer
        .write_all(&payload.data)
        .and_then(|()| writer.flush())
    {
        Ok(()) => {
            sync.mark_written(payload.sequence);
            Ok(())
        }
        Err(error) => {
            sync.mark_failed(std::io::Error::new(error.kind(), error.to_string()));
            Err(error)
        }
    }
}
type WriterThreadBody = Box<dyn FnOnce() -> std::io::Result<()> + Send>;
type WriterThreadSpawn = fn(
    std::thread::Builder,
    WriterThreadBody,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<()>>>;
/// Everything [`spawn_writer_thread`] hands back to the event loop.
pub type WriterThreadParts = (
    WriterSender,
    WriterSync,
    tokio::sync::mpsc::UnboundedReceiver<WriterEvent>,
    WriterThread,
);
/// Errors when the OS refuses to spawn. The handle must be joined during terminal teardown.
pub fn spawn_writer_thread() -> std::io::Result<WriterThreadParts> {
    spawn_writer_thread_with(|builder, body| builder.spawn(body))
}
/// [`spawn_writer_thread`] with the OS spawn injectable, so the refused-spawn path is testable.
fn spawn_writer_thread_with(spawn: WriterThreadSpawn) -> std::io::Result<WriterThreadParts> {
    let (tx, rx) = mpsc::channel::<WriterPayload>();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync = WriterSync::with_event_sender(event_tx);
    let thread_sync = sync.clone();
    let writer_thread_sync = sync.clone();
    let test_delay = std::env::var("GROK_TEST_FRAME_WRITE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis);
    let builder = std::thread::Builder::new().name("term-writer".into());
    let handle = spawn(
        builder,
        Box::new(move || -> std::io::Result<()> {
            #[cfg(not(windows))]
            let mut writer: Box<dyn std::io::Write> = {
                let tui_out = distill_tty_utils::dup_tui_stderr().unwrap_or_else(|_| {
                    use std::os::unix::io::{AsRawFd, FromRawFd};
                    let fd = unsafe { libc::dup(std::io::stderr().as_raw_fd()) };
                    unsafe { std::fs::File::from_raw_fd(fd) }
                });
                Box::new(std::io::BufWriter::with_capacity(64 * 1024, tui_out))
            };
            #[cfg(windows)]
            let mut writer: Box<dyn std::io::Write> = Box::new(std::io::BufWriter::with_capacity(
                64 * 1024,
                std::io::stderr(),
            ));
            while let Ok(payload) = rx.recv() {
                if let Some(delay) = test_delay {
                    std::thread::sleep(delay);
                }
                let result = {
                    let _guard = distill_shared::stderr::stderr_lock();
                    write_payload(&mut writer, &payload, &thread_sync)
                };
                if let Err(error) = result {
                    tracing::error!(%error, "terminal output failed");
                    return Err(error);
                }
            }
            if thread_sync.failed() {
                Err(std::io::Error::other("terminal output failed"))
            } else {
                Ok(())
            }
        }),
    )?;
    Ok((
        tx,
        sync,
        event_rx,
        WriterThread {
            handle: Some(handle),
            sync: writer_thread_sync,
        },
    ))
}
/// Tracks the last cursor position written to the terminal, so each frame emits only the cursor escapes it needs.
/// Redundant `Show`/`Hide`/`MoveTo` would reset the terminal's blink timer.
#[derive(Debug, Default)]
pub struct CursorState {
    /// `None` means the cursor is hidden; `Some((x, y))` means it is visible at (x, y).
    last_pos: Option<(u16, u16)>,
}
/// What cursor commands to emit after a frame render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    /// No cursor commands needed, so the blink timer is preserved.
    None,
    /// Cursor is visible and cells changed: reposition after cell writes disturbed the terminal cursor.
    /// Resets blink (unavoidable when cells change on screen).
    Reposition(u16, u16),
    /// Cursor becoming visible at (x, y); needs `MoveTo` and `Show`.
    Show(u16, u16),
    /// Cursor becoming hidden; needs `Hide`.
    Hide,
}
impl CursorState {
    pub fn new() -> Self {
        Self { last_pos: None }
    }
    /// Determine what cursor action to take for this frame.
    ///
    /// No side effects; call [`apply`] to execute the returned action.
    pub fn action(&self, cursor_pos: Option<(u16, u16)>, has_changes: bool) -> CursorAction {
        if cursor_pos == self.last_pos {
            if has_changes && let Some((x, y)) = cursor_pos {
                return CursorAction::Reposition(x, y);
            }
            CursorAction::None
        } else {
            match (cursor_pos, self.last_pos) {
                (Some((x, y)), Some(_)) => CursorAction::Reposition(x, y),
                (Some((x, y)), None) => CursorAction::Show(x, y),
                (None, Some(_)) => CursorAction::Hide,
                (None, None) => CursorAction::None,
            }
        }
    }
    /// `queue!` not `execute!`: cursor commands must batch with the frame and flush atomically on the writer thread.
    pub fn apply<W: Write>(&mut self, action: CursorAction, w: &mut W) {
        match action {
            CursorAction::None => {}
            CursorAction::Reposition(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                self.last_pos = Some((x, y));
            }
            CursorAction::Show(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                let _ = w.queue(cursor::Show);
                self.last_pos = Some((x, y));
            }
            CursorAction::Hide => {
                let _ = w.queue(cursor::Hide);
                self.last_pos = None;
            }
        }
    }
}
/// Bypasses ratatui `try_draw()` so cursor management stays conditional. OSC 8 spans go out before the diff, in lockstep with cells.
/// `PostFlush` stays inside the synchronized update so images appear atomically with the cell diff.
/// `ctx` decides whether the frame is wrapped in DEC 2026 at all; both markers follow that one decision.
pub fn draw_frame(
    terminal: &mut PagerTerminal,
    cursor: &mut CursorState,
    ctx: &TerminalContext,
    render_fn: impl FnOnce(
        &mut Frame,
        &mut Vec<LinkSpan>,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ),
) {
    let synchronized = should_emit_synchronized_output(ctx);
    if synchronized {
        let _ = terminal.backend_mut().queue(BeginSynchronizedUpdate);
    }
    let _ = terminal.autoresize();
    let mut link_spans: Vec<LinkSpan> = Vec::new();
    let (cursor_pos, post_flush_escapes) = {
        let mut frame = terminal.get_frame();
        render_fn(&mut frame, &mut link_spans)
    };
    terminal.set_frame_links(&link_spans);
    let has_changes = terminal.flush_with_links().unwrap_or(false);
    terminal.swap_buffers();
    let post_flush_wrote_cursor = post_flush_escapes.is_some();
    let action = cursor.action(cursor_pos, has_changes || post_flush_wrote_cursor);
    if !has_changes && !post_flush_wrote_cursor && action == CursorAction::None {
        terminal.backend_mut().writer_mut().discard();
        return;
    }
    if let Some(post_flush) = post_flush_escapes {
        let _ = post_flush.write_to(terminal.backend_mut());
    }
    cursor.apply(action, terminal.backend_mut());
    if synchronized {
        let _ = terminal.backend_mut().queue(EndSynchronizedUpdate);
    }
    let _ = terminal.backend_mut().flush();
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Frames and escapes share one queue, so the writer sees them in exact call order
    /// with strictly increasing sequences — a frame can never overtake an escape or vice versa.
    #[test]
    fn frames_and_escapes_interleave_in_call_order() {
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let mut frames = TermWriter::new(tx, sync.clone()).expect("term writer");
        let escapes = frames.escape_writer();
        frames.write_all(b"frame-1").unwrap();
        frames.flush().unwrap();
        escapes.emit("\x1b]0;title\x07");
        frames.write_all(b"frame-2").unwrap();
        frames.flush().unwrap();
        escapes.emit("\x07");
        let payloads: Vec<WriterPayload> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let bytes: Vec<&[u8]> = payloads.iter().map(WriterPayload::data).collect();
        assert_eq!(
            bytes,
            [
                b"frame-1".as_slice(),
                b"\x1b]0;title\x07",
                b"frame-2",
                b"\x07"
            ]
        );
        let sequences: Vec<u64> = payloads.iter().map(|p| p.sequence).collect();
        assert_eq!(sequences, [1, 2, 3, 4]);
        assert_eq!(sync.queued(), 4);
    }
    /// A refused OS spawn must surface as the error, not a half-built writer.
    #[test]
    fn spawn_writer_thread_propagates_refused_spawn() {
        let error = spawn_writer_thread_with(|_builder, _body| {
            Err(std::io::Error::other("no threads for you"))
        })
        .err()
        .expect("spawn failure must propagate");
        assert_eq!(error.to_string(), "no threads for you");
    }
    /// A writer thread that never exits (parked in a blocked tty write, or a sender
    /// kept alive) must not hang teardown: the bounded join detaches it.
    #[test]
    fn join_within_times_out_on_a_stuck_writer_thread() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || -> std::io::Result<()> {
            let _ = release_rx.recv();
            Ok(())
        });
        let thread = WriterThread::for_test(handle, WriterSync::new());
        let started = Instant::now();
        let outcome = thread
            .join_within(Duration::from_millis(50))
            .expect("timeout is not an error");
        assert_eq!(outcome, WriterJoin::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "join_within must return promptly at the deadline"
        );
        let _ = release_tx.send(());
    }
    /// A writer thread that drains and exits is joined normally.
    #[test]
    fn join_within_joins_a_finished_writer_thread() {
        let handle = std::thread::spawn(|| -> std::io::Result<()> { Ok(()) });
        let thread = WriterThread::for_test(handle, WriterSync::new());
        assert_eq!(
            thread.join_within(Duration::from_secs(5)).expect("join"),
            WriterJoin::Joined
        );
    }
    /// The single-producer rule is a debug assertion: a payload sent from a second
    /// thread trips it (release builds only rely on `fetch_max`).
    #[cfg(debug_assertions)]
    #[test]
    fn send_from_a_second_thread_trips_the_producer_guard() {
        let (tx, _rx) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let writer = EscapeWriter::new(tx, sync);
        writer.emit("first, latches this thread");
        let other = std::thread::spawn(move || writer.emit("second thread"));
        assert!(
            other.join().is_err(),
            "a second producer thread must trip the debug_assert"
        );
    }
    /// Escapes ride the writer queue in order and participate in the
    /// sequence protocol, so `wait_drained` covers them.
    #[test]
    fn escape_writer_enqueues_sequenced_payloads() {
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let writer = EscapeWriter::new(tx, sync.clone());
        writer.emit("\x1b[?1000h");
        writer.emit(Vec::new());
        writer.emit("\x07");
        let first = rx.try_recv().expect("first payload");
        let second = rx.try_recv().expect("second payload");
        assert!(rx.try_recv().is_err(), "empty emit must not enqueue");
        assert_eq!(first.data(), b"\x1b[?1000h");
        assert_eq!(second.data(), b"\x07");
        assert!(second.sequence > first.sequence);
        assert_eq!(sync.queued(), 2);
    }
    /// A dead writer thread must surface as a writer failure (the event loop
    /// exits on it), mirroring `TermWriter::flush`.
    #[test]
    fn escape_writer_send_failure_marks_sync_failed() {
        let (sync, mut event_rx) = WriterSync::new_for_test();
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        drop(rx);
        let writer = EscapeWriter::new(tx, sync.clone());
        writer.emit("\x07");
        assert!(sync.failed());
        assert!(matches!(event_rx.try_recv(), Ok(WriterEvent::Failed(_))));
    }
    /// A fixed 80x24 terminal whose frames land on the returned channel instead of a PTY.
    fn capturing_terminal() -> (PagerTerminal, mpsc::Receiver<WriterPayload>) {
        use ratatui::layout::Rect;
        use ratatui::{TerminalOptions, Viewport};
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        let backend = CrosstermBackend::new(
            TermWriter::new(tx, WriterSync::new()).expect("single test writer"),
        );
        let terminal = distill_ratatui_inline::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .expect("build terminal");
        (terminal, rx)
    }
    fn render_hello(
        frame: &mut ratatui::Frame,
        _links: &mut Vec<LinkSpan>,
    ) -> (
        Option<(u16, u16)>,
        Option<crate::terminal::overlay::PostFlush>,
    ) {
        frame.render_widget(
            ratatui::widgets::Paragraph::new("hello world"),
            frame.area(),
        );
        (None, None)
    }
    /// An unchanged frame must emit zero bytes to the PTY.
    #[test]
    fn idle_frame_emits_zero_bytes() {
        let (mut terminal, rx) = capturing_terminal();
        let mut cursor = CursorState::new();
        let ctx = TerminalContext::default();
        draw_frame(&mut terminal, &mut cursor, &ctx, render_hello);
        let first: Vec<u8> = rx.try_iter().flat_map(|payload| payload.data).collect();
        assert!(!first.is_empty(), "first frame should emit bytes");
        draw_frame(&mut terminal, &mut cursor, &ctx, render_hello);
        let second: Vec<u8> = rx.try_iter().flat_map(|payload| payload.data).collect();
        assert!(
            second.is_empty(),
            "idle (unchanged) frame must emit 0 bytes, got {}: {:?}",
            second.len(),
            String::from_utf8_lossy(&second),
        );
    }
    /// Begin and End are emitted as a pair or not at all, decided by the terminal context.
    #[test]
    fn synchronized_markers_follow_terminal_context() {
        use crate::terminal::{EmbeddedEditor, MultiplexerKind};
        const BEGIN: &[u8] = b"\x1b[?2026h";
        const END: &[u8] = b"\x1b[?2026l";
        fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            haystack.windows(needle.len()).position(|w| w == needle)
        }
        let tmux = TerminalContext {
            multiplexer: MultiplexerKind::Tmux,
            ..Default::default()
        };
        let editor_inside_tmux = TerminalContext {
            multiplexer: MultiplexerKind::Tmux,
            embedded_editor: Some(EmbeddedEditor::Neovim),
            ..Default::default()
        };
        let cases = [
            (TerminalContext::default(), true),
            (tmux, false),
            (editor_inside_tmux, true),
        ];
        for (ctx, wrapped) in &cases {
            let (mut terminal, rx) = capturing_terminal();
            let mut cursor = CursorState::new();
            draw_frame(&mut terminal, &mut cursor, ctx, render_hello);
            let frame: Vec<u8> = rx.try_iter().flat_map(|payload| payload.data).collect();
            assert!(
                frame.windows(b"hello".len()).any(|w| w == b"hello"),
                "frame must carry the cell diff"
            );
            let (begin, end) = (find(&frame, BEGIN), find(&frame, END));
            if *wrapped {
                assert!(
                    matches!((begin, end), (Some(b), Some(e)) if b < e),
                    "{ctx:?}: expected Begin before End, got {:?}",
                    String::from_utf8_lossy(&frame)
                );
            } else {
                assert_eq!(
                    (begin, end),
                    (None, None),
                    "{ctx:?}: expected no DEC 2026 markers, got {:?}",
                    String::from_utf8_lossy(&frame)
                );
            }
        }
    }
    #[test]
    fn writer_success_is_acknowledged_after_flush() {
        let (sync, mut events) = WriterSync::new_for_test();
        let sequence = sync.reserve_sequence();
        let payload = WriterPayload {
            sequence,
            data: b"frame bytes".to_vec(),
        };
        let mut sink = Vec::new();
        write_payload(&mut sink, &payload, &sync).expect("write payload");
        assert_eq!(sink, b"frame bytes");
        assert_eq!(sync.written(), sequence);
        assert!(matches!(
            events.try_recv(),
            Ok(WriterEvent::Written(written)) if written == sequence
        ));
        assert_eq!(
            sync.wait_drained(Duration::from_secs(1)).unwrap(),
            WriterDrain::Drained
        );
    }
    #[test]
    fn writer_flush_failure_is_not_acknowledged() {
        struct FlushFailWriter {
            data: Vec<u8>,
        }
        impl Write for FlushFailWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.data.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush failed"))
            }
        }
        let (sync, mut events) = WriterSync::new_for_test();
        let sequence = sync.reserve_sequence();
        let payload = WriterPayload {
            sequence,
            data: b"frame bytes".to_vec(),
        };
        let mut sink = FlushFailWriter { data: Vec::new() };
        assert!(write_payload(&mut sink, &payload, &sync).is_err());
        assert_eq!(sink.data, b"frame bytes");
        assert_eq!(sync.written(), 0);
        assert!(sync.failed());
        assert!(matches!(events.try_recv(), Ok(WriterEvent::Failed(_))));
    }
    #[test]
    fn writer_failure_is_not_acknowledged() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("write failed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (sync, mut events) = WriterSync::new_for_test();
        let sequence = sync.reserve_sequence();
        let payload = WriterPayload {
            sequence,
            data: b"frame bytes".to_vec(),
        };
        assert!(write_payload(&mut FailingWriter, &payload, &sync).is_err());
        assert_eq!(sync.written(), 0);
        assert!(sync.failed());
        assert!(matches!(events.try_recv(), Ok(WriterEvent::Failed(_))));
        assert!(sync.wait_drained(Duration::from_secs(1)).is_err());
    }
    /// Out-of-order acks must keep the watermark monotonic: a plain store would
    /// regress it below `queued` and wedge `wait_drained`/`acknowledge` forever.
    #[test]
    fn out_of_order_acks_keep_watermark_monotonic() {
        let sync = WriterSync::new();
        let first = sync.reserve_sequence();
        let second = sync.reserve_sequence();
        sync.mark_written(second);
        sync.mark_written(first);
        assert_eq!(sync.written(), second);
        assert!(first < second);
        assert_eq!(
            sync.wait_drained(Duration::ZERO).unwrap(),
            WriterDrain::Drained
        );
    }
    #[test]
    fn writer_drain_timeout_is_bounded_and_retryable() {
        let sync = WriterSync::new();
        let sequence = sync.reserve_sequence();
        let started = Instant::now();
        assert_eq!(
            sync.wait_drained(Duration::from_millis(5)).unwrap(),
            WriterDrain::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        sync.mark_written(sequence);
        assert_eq!(
            sync.wait_drained(Duration::ZERO).unwrap(),
            WriterDrain::Drained
        );
    }
    #[test]
    fn term_writer_send_failure_is_published_and_not_acknowledged() {
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        drop(rx);
        let (sync, mut events) = WriterSync::new_for_test();
        let mut writer = TermWriter::new(tx, sync.clone()).expect("single test writer");
        writer.write_all(b"frame bytes").expect("buffer write");
        assert!(writer.flush().is_err());
        assert_eq!(sync.queued(), 1);
        assert_eq!(sync.written(), 0);
        assert!(matches!(events.try_recv(), Ok(WriterEvent::Failed(_))));
    }
    #[test]
    fn writer_sync_rejects_multiple_live_producers() {
        let (tx, _rx) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let first = TermWriter::new(tx.clone(), sync.clone()).expect("first writer");
        assert!(matches!(
            TermWriter::new(tx.clone(), sync.clone()),
            Err(WriterAlreadyActive)
        ));
        drop(first);
        assert!(TermWriter::new(tx, sync).is_ok());
    }
    #[test]
    fn drain_observes_reservation_before_payload_is_consumed() {
        let (tx, rx) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let mut writer = TermWriter::new(tx, sync.clone()).expect("single test writer");
        writer.write_all(b"frame bytes").expect("buffer write");
        writer.flush().expect("flush");
        assert_eq!(sync.queued(), 1);
        assert!(!sync.is_drained());
        assert_eq!(rx.recv().expect("payload").sequence, 1);
    }
    fn state_hidden() -> CursorState {
        CursorState { last_pos: None }
    }
    fn state_at(x: u16, y: u16) -> CursorState {
        CursorState {
            last_pos: Some((x, y)),
        }
    }
    #[test]
    fn hidden_no_changes_stays_hidden() {
        let s = state_hidden();
        assert_eq!(s.action(None, false), CursorAction::None);
    }
    #[test]
    fn visible_same_pos_no_changes_preserves_blink() {
        let s = state_at(5, 10);
        assert_eq!(s.action(Some((5, 10)), false), CursorAction::None);
    }
    #[test]
    fn visible_new_pos_no_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((6, 10)), false),
            CursorAction::Reposition(6, 10)
        );
    }
    #[test]
    fn hidden_with_changes_stays_hidden() {
        let s = state_hidden();
        assert_eq!(s.action(None, true), CursorAction::None);
    }
    #[test]
    fn visible_same_pos_with_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((5, 10)), true),
            CursorAction::Reposition(5, 10)
        );
    }
    #[test]
    fn visible_new_pos_with_changes_repositions() {
        let s = state_at(5, 10);
        assert_eq!(
            s.action(Some((8, 10)), true),
            CursorAction::Reposition(8, 10)
        );
    }
    #[test]
    fn hidden_to_visible_shows() {
        let s = state_hidden();
        assert_eq!(s.action(Some((5, 10)), false), CursorAction::Show(5, 10));
    }
    #[test]
    fn hidden_to_visible_with_changes_shows() {
        let s = state_hidden();
        assert_eq!(s.action(Some((5, 10)), true), CursorAction::Show(5, 10));
    }
    #[test]
    fn visible_to_hidden_hides() {
        let s = state_at(5, 10);
        assert_eq!(s.action(None, false), CursorAction::Hide);
    }
    #[test]
    fn visible_to_hidden_with_changes_hides() {
        let s = state_at(5, 10);
        assert_eq!(s.action(None, true), CursorAction::Hide);
    }
    #[test]
    fn apply_show_updates_last_pos() {
        let mut s = state_hidden();
        let mut sink = Vec::new();
        s.apply(CursorAction::Show(3, 7), &mut sink);
        assert_eq!(s.last_pos, Some((3, 7)));
    }
    #[test]
    fn apply_hide_clears_last_pos() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::Hide, &mut sink);
        assert_eq!(s.last_pos, None);
    }
    #[test]
    fn apply_reposition_updates_last_pos() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::Reposition(5, 9), &mut sink);
        assert_eq!(s.last_pos, Some((5, 9)));
    }
    #[test]
    fn apply_none_preserves_state() {
        let mut s = state_at(3, 7);
        let mut sink = Vec::new();
        s.apply(CursorAction::None, &mut sink);
        assert_eq!(s.last_pos, Some((3, 7)));
    }
    /// Verify the writer thread correctly round-trips multi-byte UTF-8 through the channel.
    /// This catches encoding issues where the writer silently corrupts Braille/emoji/CJK characters.
    #[test]
    fn writer_thread_preserves_multibyte_utf8() {
        let test_payload = "⣀⣾⠿⠛\u{e0a0}\u{1F600}";
        let expected_bytes = test_payload.as_bytes().to_vec();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture2 = capture.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            while let Ok(data) = rx.recv() {
                buf.extend_from_slice(&data);
            }
            *capture2.lock().unwrap() = buf;
        });
        tx.send(expected_bytes.clone()).unwrap();
        drop(tx);
        handle.join().unwrap();
        let captured = capture.lock().unwrap();
        assert_eq!(
            *captured, expected_bytes,
            "Writer thread corrupted multi-byte UTF-8 payload"
        );
        assert_eq!(
            std::str::from_utf8(&captured).unwrap(),
            test_payload,
            "Round-tripped bytes do not decode to original UTF-8 string"
        );
    }
}
