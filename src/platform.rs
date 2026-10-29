//! Platform layer: the only place Windows APIs are touched.
//! It implements [`crate::InstanceBackend`] — the OS half of
//! the acquire state machine in `lib.rs`:
//!
//! - `create_mutex` runs the election with `CreateMutexW`.
//!   A secondary closes its handle to the existing mutex
//!   immediately: a secondary that held it open would keep
//!   the name alive after the primary exits, and the next
//!   starter would forward to a pipe nobody serves.
//! - `start_listener` spawns the pipe-server thread. Every
//!   wait in that thread includes a stop event, so
//!   `PrimaryHandle::release` can always stop and join it —
//!   release never detaches a thread.
//! - `forward` connects to the primary's pipe (retrying
//!   while the pipe is still coming up), writes one frame,
//!   and waits for the ack frame against the caller's
//!   deadline.
//!
//! All pipe I/O is overlapped; every extern block carries
//! `#[link(name = "kernel32")]` — without it the build
//! fails at LINK time with LNK2019, which check and clippy
//! cannot catch; CI's real build can.
//!
//! On non-Windows targets the backend reports
//! `ERR_NOT_SUPPORTED`, so the pure logic and the FFI
//! contract stay testable anywhere.

#[cfg(not(windows))]
use crate::ERR_NOT_SUPPORTED;
use crate::{InResult, InstanceBackend, Mailbox, MutexAttempt, Scope};
use std::sync::Arc;

/// Owns a live primary's platform resources: the election
/// mutex handle and the listener thread (Windows). Off
/// Windows it is an empty shell — no primary can exist
/// there.
pub struct PrimaryHandle {
    #[cfg(windows)]
    inner: WinPrimary,
}

#[cfg(windows)]
struct WinPrimary {
    mutex: isize,
    stop: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PrimaryHandle {
    /// A handle that owns nothing (tests, and primaries in
    /// name only). `release` on it is a no-op.
    pub fn detached() -> Self {
        PrimaryHandle {
            #[cfg(windows)]
            inner: WinPrimary {
                mutex: 0,
                stop: 0,
                thread: None,
            },
        }
    }

    /// Tear the primary down: stop and JOIN the listener
    /// thread, then close the stop event and the mutex.
    /// Closing the mutex is what frees the name — the next
    /// `in_acquire` for it becomes the new primary.
    pub fn release(self) {
        #[cfg(windows)]
        {
            let inner = self.inner;
            if inner.stop != 0 {
                // SAFETY: the handle is ours and stays open
                // until after the join below.
                unsafe { win::SetEvent(inner.stop) };
            }
            if let Some(thread) = inner.thread {
                let _ = thread.join();
            }
            if inner.stop != 0 {
                // SAFETY: our handle; the listener thread
                // has exited, so nothing else can use it.
                unsafe { win::CloseHandle(inner.stop) };
            }
            if inner.mutex != 0 {
                // SAFETY: our handle, opened by create_mutex.
                unsafe { win::CloseHandle(inner.mutex) };
            }
        }
    }
}

/// The acquire backend. On Windows it briefly holds the
/// election mutex handle between `create_mutex` and
/// `start_listener`; dropping the backend closes a handle
/// that was never handed off (a listener that failed to
/// start must not leave the name claimed).
pub struct Backend {
    #[cfg(windows)]
    mutex: isize,
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            #[cfg(windows)]
            mutex: 0,
        }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Drop for Backend {
    fn drop(&mut self) {
        if self.mutex != 0 {
            // SAFETY: our handle, never handed off.
            unsafe { win::CloseHandle(self.mutex) };
        }
    }
}

#[cfg(not(windows))]
impl InstanceBackend for Backend {
    fn create_mutex(&mut self, _name: &str, _scope: Scope) -> InResult<MutexAttempt> {
        Err((
            ERR_NOT_SUPPORTED,
            "single-instance acquire is only available on Windows".to_string(),
        ))
    }

    fn start_listener(&mut self, _name: &str, _mailbox: Arc<Mailbox>) -> InResult<PrimaryHandle> {
        Err((
            ERR_NOT_SUPPORTED,
            "single-instance listener is only available on Windows".to_string(),
        ))
    }

    fn forward(&mut self, _name: &str, _frame: &[u8], _timeout_ms: u64) -> bool {
        false
    }
}

#[cfg(windows)]
impl InstanceBackend for Backend {
    fn create_mutex(&mut self, name: &str, scope: Scope) -> InResult<MutexAttempt> {
        let full = win::wide(&crate::mutex_name(name, scope));
        // SAFETY: `full` is a NUL-terminated UTF-16 name
        // that outlives the call; no security descriptor
        // (NULL = default), not initially owned — existence
        // is the election, ownership is never taken, so an
        // abandoned-mutex state can never wedge the name.
        let handle = unsafe { win::CreateMutexW(std::ptr::null_mut(), 0, full.as_ptr()) };
        if handle == 0 {
            // SAFETY: read immediately after the failing call.
            let err = unsafe { win::GetLastError() };
            return Err(win::map_error(err, "CreateMutexW"));
        }
        // SAFETY: GetLastError is meaningful only read
        // immediately after CreateMutexW returns.
        let err = unsafe { win::GetLastError() };
        if err == win::ERROR_ALREADY_EXISTS {
            // Secondary: close our handle NOW (see the
            // module docs — holding it would poison the
            // name after the primary exits).
            // SAFETY: our handle, closed exactly once.
            unsafe { win::CloseHandle(handle) };
            return Ok(MutexAttempt::AlreadyExists);
        }
        self.mutex = handle;
        Ok(MutexAttempt::Created)
    }

    fn start_listener(&mut self, name: &str, mailbox: Arc<Mailbox>) -> InResult<PrimaryHandle> {
        // SAFETY: manual-reset event, initially unset,
        // unnamed. Owned by the returned PrimaryHandle.
        let stop = unsafe { win::CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if stop == 0 {
            // SAFETY: read immediately after the failing call.
            let err = unsafe { win::GetLastError() };
            return Err(win::map_error(err, "CreateEventW"));
        }
        let pipe = crate::pipe_name(name);
        let thread = std::thread::spawn(move || win::listener_loop(&pipe, mailbox, stop));
        let mutex = std::mem::replace(&mut self.mutex, 0);
        Ok(PrimaryHandle {
            inner: WinPrimary {
                mutex,
                stop,
                thread: Some(thread),
            },
        })
    }

    fn forward(&mut self, name: &str, frame: &[u8], timeout_ms: u64) -> bool {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(1));
        let path = win::wide(&crate::pipe_name(name));
        // Connect, retrying while the pipe is still coming
        // up (the primary creates its listener just after
        // winning the election) or busy with another
        // secondary — always bounded by the deadline.
        let pipe = loop {
            // SAFETY: `path` is NUL-terminated UTF-16 that
            // outlives the call; overlapped flag because all
            // I/O below is overlapped.
            let handle = unsafe {
                win::CreateFileW(
                    path.as_ptr(),
                    win::GENERIC_READ | win::GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    win::OPEN_EXISTING,
                    win::FILE_FLAG_OVERLAPPED,
                    0,
                )
            };
            if handle != win::INVALID_HANDLE_VALUE {
                break handle;
            }
            // SAFETY: read immediately after the failing call.
            let err = unsafe { win::GetLastError() };
            if std::time::Instant::now() >= deadline {
                return false;
            }
            match err {
                win::ERROR_PIPE_BUSY => {
                    // SAFETY: waits for a free pipe instance,
                    // bounded by the remaining time.
                    unsafe { win::WaitNamedPipeW(path.as_ptr(), win::ms_until(deadline)) };
                }
                win::ERROR_FILE_NOT_FOUND | win::ERROR_PATH_NOT_FOUND => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                // Access denied and anything else: the ack
                // can never arrive — report honestly.
                _ => return false,
            }
        };
        let ok = win::client_exchange(pipe, frame, deadline);
        // SAFETY: our handle from CreateFileW, closed once.
        unsafe { win::CloseHandle(pipe) };
        ok
    }
}

// ---------------------------------------------------------------------------
// Win32 bindings and the pipe machinery
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::*;
    use crate::{encode_frame, is_ack_frame, parse_forward_frame, FrameDecoder, ACK_FRAME_JSON};
    use std::os::raw::c_void;

    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const GENERIC_WRITE: u32 = 0x4000_0000;
    pub const OPEN_EXISTING: u32 = 3;
    pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    pub const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    pub const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    pub const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    pub const PIPE_WAIT: u32 = 0x0000_0000;
    pub const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    pub const INVALID_HANDLE_VALUE: isize = -1;
    pub const INFINITE: u32 = 0xFFFF_FFFF;
    pub const WAIT_OBJECT_0: u32 = 0;
    pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
    pub const ERROR_ACCESS_DENIED: u32 = 5;
    pub const ERROR_FILE_NOT_FOUND: u32 = 2;
    pub const ERROR_PATH_NOT_FOUND: u32 = 3;
    pub const ERROR_BROKEN_PIPE: u32 = 109;
    pub const ERROR_ALREADY_EXISTS: u32 = 183;
    pub const ERROR_PIPE_BUSY: u32 = 231;
    pub const ERROR_PIPE_CONNECTED: u32 = 535;
    pub const ERROR_IO_PENDING: u32 = 997;

    /// The Win32 OVERLAPPED layout (the pointer-sized
    /// union member is represented by its offset pair).
    #[repr(C)]
    pub struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: isize,
    }

    impl Overlapped {
        fn new(event: isize) -> Self {
            Overlapped {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                event,
            }
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> isize;
        pub fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            default_timeout: u32,
            attrs: *const c_void,
        ) -> isize;
        pub fn ConnectNamedPipe(pipe: isize, overlapped: *mut Overlapped) -> i32;
        pub fn DisconnectNamedPipe(pipe: isize) -> i32;
        pub fn CreateFileW(
            name: *const u16,
            access: u32,
            share_mode: u32,
            attrs: *const c_void,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        pub fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
        pub fn ReadFile(
            file: isize,
            buffer: *mut c_void,
            to_read: u32,
            read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        pub fn WriteFile(
            file: isize,
            buffer: *const c_void,
            to_write: u32,
            written: *mut u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        pub fn FlushFileBuffers(file: isize) -> i32;
        pub fn CloseHandle(object: isize) -> i32;
        pub fn GetLastError() -> u32;
        pub fn CreateEventW(
            attrs: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> isize;
        pub fn SetEvent(event: isize) -> i32;
        pub fn WaitForSingleObject(handle: isize, timeout_ms: u32) -> u32;
        pub fn WaitForMultipleObjects(
            count: u32,
            handles: *const isize,
            wait_all: i32,
            timeout_ms: u32,
        ) -> u32;
        pub fn GetOverlappedResult(
            file: isize,
            overlapped: *mut Overlapped,
            transferred: *mut u32,
            wait: i32,
        ) -> i32;
        pub fn CancelIoEx(file: isize, overlapped: *mut Overlapped) -> i32;
    }

    /// NUL-terminated UTF-16 for a Win32 name/path.
    pub fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn map_error(err: u32, what: &str) -> (i32, String) {
        let code = if err == ERROR_ACCESS_DENIED {
            crate::ERR_ACCESS_DENIED
        } else {
            crate::ERR_PLATFORM
        };
        (code, format!("{what} failed with Win32 error {err}"))
    }

    /// Milliseconds from now until `deadline` (0 when past).
    pub fn ms_until(deadline: std::time::Instant) -> u32 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        left.as_millis().min(u32::MAX as u128) as u32
    }

    fn stop_signalled(stop: isize) -> bool {
        // SAFETY: `stop` is a live event handle owned by the
        // PrimaryHandle for the listener's whole lifetime.
        unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
    }

    /// Outcome of one overlapped pipe operation.
    enum IoOutcome {
        Done(usize),
        Stopped,
        TimedOut,
        Eof,
        Failed,
    }

    /// Issue ONE overlapped read or write on `pipe` and wait
    /// for it. `stop != 0` adds the stop event to the wait
    /// (listener side, no deadline); `deadline` bounds the
    /// wait (client side, no stop event). On stop/timeout
    /// the operation is cancelled and reaped before
    /// returning, so the pipe handle stays safe to close or
    /// reuse.
    ///
    /// SAFETY: `buf` must be valid for `len` bytes (writable
    /// for reads, readable for writes) and outlive the wait;
    /// `pipe` must be a handle opened with
    /// FILE_FLAG_OVERLAPPED.
    unsafe fn pipe_io(
        pipe: isize,
        write: bool,
        buf: *mut u8,
        len: u32,
        stop: isize,
        deadline: Option<std::time::Instant>,
    ) -> IoOutcome {
        let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        if event == 0 {
            return IoOutcome::Failed;
        }
        let mut ov = Overlapped::new(event);
        let issued = if write {
            WriteFile(
                pipe,
                buf as *const c_void,
                len,
                std::ptr::null_mut(),
                &mut ov,
            )
        } else {
            ReadFile(pipe, buf as *mut c_void, len, std::ptr::null_mut(), &mut ov)
        };
        let outcome = complete_io(pipe, &mut ov, event, issued, stop, deadline);
        CloseHandle(event);
        outcome
    }

    /// Complete an issued overlapped operation: fetch the
    /// result if it finished synchronously, otherwise wait
    /// (stop event and/or deadline) and then fetch it.
    ///
    /// SAFETY: same contract as `pipe_io`; `ov` and `event`
    /// belong to the issued operation.
    unsafe fn complete_io(
        pipe: isize,
        ov: *mut Overlapped,
        event: isize,
        issued: i32,
        stop: isize,
        deadline: Option<std::time::Instant>,
    ) -> IoOutcome {
        if issued == 0 {
            let err = GetLastError();
            if err == ERROR_BROKEN_PIPE {
                return IoOutcome::Eof;
            }
            if err != ERROR_IO_PENDING {
                return IoOutcome::Failed;
            }
            let wait = if stop != 0 {
                let handles = [event, stop];
                WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE)
            } else {
                let ms = deadline.map(ms_until).unwrap_or(INFINITE);
                WaitForSingleObject(event, ms)
            };
            if stop != 0 && wait == WAIT_OBJECT_0 + 1 {
                CancelIoEx(pipe, ov);
                let mut sink: u32 = 0;
                let _ = GetOverlappedResult(pipe, ov, &mut sink, 1);
                return IoOutcome::Stopped;
            }
            if wait == WAIT_TIMEOUT {
                CancelIoEx(pipe, ov);
                let mut sink: u32 = 0;
                let _ = GetOverlappedResult(pipe, ov, &mut sink, 1);
                return IoOutcome::TimedOut;
            }
            if wait != WAIT_OBJECT_0 {
                return IoOutcome::Failed;
            }
        }
        let mut transferred: u32 = 0;
        if GetOverlappedResult(pipe, ov, &mut transferred, 0) != 0 {
            if transferred == 0 {
                IoOutcome::Eof
            } else {
                IoOutcome::Done(transferred as usize)
            }
        } else if GetLastError() == ERROR_BROKEN_PIPE {
            IoOutcome::Eof
        } else {
            IoOutcome::Failed
        }
    }

    // -- Listener (primary side) --

    enum ConnectOutcome {
        Connected,
        Stopped,
        Failed,
    }

    /// The pipe-server thread: create an instance, wait for
    /// one secondary, serve its one frame, repeat — until
    /// the stop event fires. Every blocking wait includes
    /// the stop event, so `release` can always join this
    /// thread.
    pub fn listener_loop(pipe_path: &str, mailbox: Arc<Mailbox>, stop: isize) {
        let wide_path = wide(pipe_path);
        loop {
            if stop_signalled(stop) {
                return;
            }
            // SAFETY: `wide_path` outlives the call and the
            // thread; the returned handle is this loop's to
            // close (in serve_instance or on the stop paths).
            let pipe = unsafe {
                CreateNamedPipeW(
                    wide_path.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    65_536,
                    65_536,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                // Transient (or a torn-down namespace): nap
                // on the stop event and retry — but never
                // spin, and never outlive a stop.
                // SAFETY: stop is a live event handle.
                if unsafe { WaitForSingleObject(stop, 100) } == WAIT_OBJECT_0 {
                    return;
                }
                continue;
            }
            // SAFETY: `pipe` is this loop's fresh overlapped
            // instance and `stop` is a live event handle.
            match unsafe { connect_instance(pipe, stop) } {
                ConnectOutcome::Connected => serve_instance(pipe, &mailbox, stop),
                ConnectOutcome::Stopped => {
                    // SAFETY: our instance handle, unconnected.
                    unsafe { CloseHandle(pipe) };
                    return;
                }
                ConnectOutcome::Failed => {
                    // SAFETY: our instance handle.
                    unsafe { CloseHandle(pipe) };
                }
            }
        }
    }

    /// Wait (overlapped, stop-aware) for one client to
    /// connect to this pipe instance.
    ///
    /// SAFETY: `pipe` is a fresh overlapped pipe instance;
    /// `stop` is a live event handle.
    unsafe fn connect_instance(pipe: isize, stop: isize) -> ConnectOutcome {
        let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        if event == 0 {
            return ConnectOutcome::Failed;
        }
        let mut ov = Overlapped::new(event);
        let issued = ConnectNamedPipe(pipe, &mut ov);
        let outcome = if issued != 0 {
            ConnectOutcome::Connected
        } else {
            match GetLastError() {
                ERROR_PIPE_CONNECTED => ConnectOutcome::Connected,
                ERROR_IO_PENDING => {
                    let handles = [event, stop];
                    let wait = WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE);
                    if wait == WAIT_OBJECT_0 {
                        let mut sink: u32 = 0;
                        if GetOverlappedResult(pipe, &mut ov, &mut sink, 0) != 0 {
                            ConnectOutcome::Connected
                        } else {
                            ConnectOutcome::Failed
                        }
                    } else if wait == WAIT_OBJECT_0 + 1 {
                        CancelIoEx(pipe, &mut ov);
                        let mut sink: u32 = 0;
                        let _ = GetOverlappedResult(pipe, &mut ov, &mut sink, 1);
                        ConnectOutcome::Stopped
                    } else {
                        ConnectOutcome::Failed
                    }
                }
                _ => ConnectOutcome::Failed,
            }
        };
        CloseHandle(event);
        outcome
    }

    /// Serve one connected secondary: read until one
    /// complete frame arrives; a well-formed forward frame
    /// is queued in the mailbox and acked. Anything else
    /// (malformed, oversized, EOF, stop) ends the session
    /// WITHOUT an ack — the secondary then honestly reports
    /// `forwarded: false`. The instance is disconnected and
    /// closed on every path.
    fn serve_instance(pipe: isize, mailbox: &Mailbox, stop: isize) {
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: `buf` outlives the wait; `pipe` is our
            // connected overlapped instance.
            let outcome =
                unsafe { pipe_io(pipe, false, buf.as_mut_ptr(), buf.len() as u32, stop, None) };
            match outcome {
                IoOutcome::Done(n) => {
                    decoder.feed(&buf[..n]);
                    match decoder.next_frame() {
                        Ok(Some(payload)) => {
                            if let Ok(args) = parse_forward_frame(&payload) {
                                mailbox.push(args);
                                if let Ok(ack) = encode_frame(ACK_FRAME_JSON.as_bytes()) {
                                    // SAFETY: `ack` outlives the
                                    // write; pipe contract as above.
                                    let _ = unsafe {
                                        pipe_io(
                                            pipe,
                                            true,
                                            ack.as_ptr() as *mut u8,
                                            ack.len() as u32,
                                            stop,
                                            None,
                                        )
                                    };
                                    // The ack must reach the
                                    // client before disconnect.
                                    // SAFETY: connected instance.
                                    unsafe { FlushFileBuffers(pipe) };
                                }
                            }
                            break;
                        }
                        Ok(None) => continue, // frame still arriving
                        Err(_) => break,      // oversized/desynced stream
                    }
                }
                _ => break, // EOF, stop, or failure: session over
            }
        }
        // SAFETY: our connected instance; disconnect then
        // close, exactly once, on every path out of serve.
        unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    }

    // -- Client (secondary side) --

    /// Write the forward frame, then read until the ack
    /// frame completes — all bounded by `deadline`. True
    /// only when a well-formed `{"ack":true}` frame arrived.
    pub fn client_exchange(pipe: isize, frame: &[u8], deadline: std::time::Instant) -> bool {
        let mut written = 0;
        while written < frame.len() {
            // SAFETY: the slice outlives the wait; the write
            // never mutates through the pointer (WriteFile
            // only reads it) — the cast is the API's shape.
            let outcome = unsafe {
                pipe_io(
                    pipe,
                    true,
                    frame[written..].as_ptr() as *mut u8,
                    (frame.len() - written) as u32,
                    0,
                    Some(deadline),
                )
            };
            match outcome {
                IoOutcome::Done(n) => written += n,
                _ => return false,
            }
        }
        // SAFETY: our connected client handle; flushing
        // pushes the frame out before we start waiting.
        unsafe { FlushFileBuffers(pipe) };
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `buf` outlives the wait; pipe contract
            // as in pipe_io.
            let outcome = unsafe {
                pipe_io(
                    pipe,
                    false,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    0,
                    Some(deadline),
                )
            };
            match outcome {
                IoOutcome::Done(n) => {
                    decoder.feed(&buf[..n]);
                    match decoder.next_frame() {
                        Ok(Some(payload)) => return is_ack_frame(&payload),
                        Ok(None) => continue,
                        Err(_) => return false,
                    }
                }
                _ => return false,
            }
        }
    }
}
