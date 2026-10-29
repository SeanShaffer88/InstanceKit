//! InstanceKit — single-instance enforcement with argument
//! forwarding, behind a small C ABI.
//!
//! The problem this solves: every desktop app in the family
//! hand-rolled the same trick — create a named mutex; if it
//! already exists, a second copy of the app is starting, so
//! hand the running copy whatever the user asked for (the
//! file they double-clicked, the link they clicked) and
//! exit. The hand-rolled versions each invented their own
//! IPC, their own framing, and their own answer to "how do
//! I know the primary actually got it". InstanceKit is the
//! one implementation:
//!
//! - **The mutex is the election.** `in_acquire` creates
//!   `Local\<name>` (or `Global\<name>`). The process that
//!   creates it is the primary; a process that finds it
//!   already existing is a secondary. The kernel guarantees
//!   exactly one creator, so there is no race to arbitrate.
//! - **Forwarding is acknowledged.** A secondary with
//!   `forward_args` connects to the primary's named pipe
//!   `\\.\pipe\<name>-instance`, sends one length-prefixed
//!   JSON frame `{"args": ...}`, and waits for the primary's
//!   ack frame. `forwarded` in the result is true only when
//!   the ack arrived — never assumed.
//! - **The primary polls.** Forwarded arguments queue in
//!   arrival order; `in_poll` returns the next one or
//!   `{"args": null}` when its timeout elapses.
//! - **Release is total.** `in_release` stops the listener
//!   thread and joins it, closes the pipe and mutex
//!   handles, and wakes any blocked poll. A later
//!   `in_acquire` for the same name becomes the new
//!   primary.
//!
//! Layout: this module is the pure core — name validation,
//! the frame codec, the message envelope, spec parsing,
//! the primary/secondary state machine (driven through the
//! [`InstanceBackend`] trait so it is fully testable on any
//! OS), the mailbox and the token registry. `platform`
//! holds the Win32 backend; `ffi` is the C ABI. Off
//! Windows, acquire reports `ERR_NOT_SUPPORTED` and the
//! rest of the contract stays testable.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/instancekit.h).
pub const IN_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;

pub type InResult<T> = Result<T, (i32, String)>;

/// Default `forward_timeout_ms` for `in_acquire`.
pub const DEFAULT_FORWARD_TIMEOUT_MS: u64 = 3000;
/// Hard cap on `in_poll`'s `timeout_ms`.
pub const MAX_POLL_TIMEOUT_MS: u64 = 60_000;
/// Largest frame payload the wire format carries: 1 MiB.
pub const MAX_FRAME_PAYLOAD: usize = 1_048_576;

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock means a panic happened while holding
    // it; the state is still structurally valid, so recover
    // the guard rather than wedging every later call.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// The mutex / pipe namespace a name lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `Local\` — one primary per login session.
    Local,
    /// `Global\` — one primary per machine.
    Global,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::Global => "global",
        }
    }
}

/// Validate an instance name: `[A-Za-z0-9._-]{1,64}`. The
/// restricted alphabet is what makes the name safe to embed
/// verbatim in a mutex name and a pipe path — no separators,
/// no whitespace, no characters either namespace treats
/// specially.
pub fn validate_name(name: &str) -> InResult<()> {
    if name.is_empty() {
        return Err((ERR_INVALID_ARG, "\"name\" must not be empty".to_string()));
    }
    if name.chars().count() > 64 {
        return Err((
            ERR_INVALID_ARG,
            "\"name\" must be at most 64 characters".to_string(),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err((
                ERR_INVALID_ARG,
                format!("\"name\" contains a character outside [A-Za-z0-9._-]: '{c}'"),
            ));
        }
    }
    Ok(())
}

/// The kernel mutex name for an instance: `Local\<name>`
/// or `Global\<name>`.
pub fn mutex_name(name: &str, scope: Scope) -> String {
    match scope {
        Scope::Local => format!("Local\\{name}"),
        Scope::Global => format!("Global\\{name}"),
    }
}

/// The named-pipe path for an instance:
/// `\\.\pipe\<name>-instance` (the same in both scopes —
/// the pipe namespace is per-machine; scope separates the
/// mutex election, and a secondary only ever reaches the
/// pipe after losing that election in its scope).
pub fn pipe_name(name: &str) -> String {
    format!(r"\\.\pipe\{name}-instance")
}

// ---------------------------------------------------------------------------
// Frame codec — 4-byte little-endian length prefix + payload
// ---------------------------------------------------------------------------

/// Encode one frame. Payloads above [`MAX_FRAME_PAYLOAD`]
/// are rejected, so a sender can never make a receiver
/// buffer without bound.
pub fn encode_frame(payload: &[u8]) -> InResult<Vec<u8>> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err((
            ERR_INVALID_ARG,
            format!(
                "frame payload of {} bytes exceeds the {MAX_FRAME_PAYLOAD}-byte limit",
                payload.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Incremental frame decoder: feed bytes as they arrive,
/// pull complete frames out. A declared length above
/// [`MAX_FRAME_PAYLOAD`] is an error the moment the header
/// completes — the decoder never waits for (or buffers)
/// an oversized payload.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder { buf: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame's payload, or `None` when
    /// more bytes are needed. After an `Err` the stream is
    /// desynchronized; the caller must drop the connection.
    pub fn next_frame(&mut self) -> InResult<Option<Vec<u8>>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME_PAYLOAD {
            return Err((
                ERR_PLATFORM,
                format!(
                    "frame header declares {len} bytes, over the {MAX_FRAME_PAYLOAD}-byte limit"
                ),
            ));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(payload))
    }

    /// Bytes buffered toward an incomplete frame.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// Message envelope
// ---------------------------------------------------------------------------

/// The frame a secondary sends: `{"args": <forward_args>}`.
pub fn forward_frame_json(args: &json::Value) -> String {
    let mut out = String::with_capacity(16 + 64);
    out.push_str("{\"args\":");
    out.push_str(&args.to_json());
    out.push('}');
    out
}

/// The frame the primary answers with once the message is
/// queued: `{"ack": true}`.
pub const ACK_FRAME_JSON: &str = "{\"ack\":true}";

/// Extract the forwarded args from a frame the primary
/// received. Anything that is not a JSON object carrying
/// an `"args"` key is a protocol error (no ack is sent for
/// it — the sender must not believe it was delivered).
pub fn parse_forward_frame(payload: &[u8]) -> InResult<json::Value> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| (ERR_PARSE, "frame payload is not valid UTF-8".to_string()))?;
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("frame is not valid JSON: {e}")))?;
    if value.as_object().is_none() {
        return Err((ERR_PARSE, "frame must be a JSON object".to_string()));
    }
    value.get("args").cloned().ok_or((
        ERR_PARSE,
        "frame object carries no \"args\" key".to_string(),
    ))
}

/// True only for a well-formed `{"ack": true}` frame.
pub fn is_ack_frame(payload: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(payload) else {
        return false;
    };
    let Ok(value) = json::parse(text) else {
        return false;
    };
    value.get("ack").and_then(|v| v.as_bool()) == Some(true)
}

// ---------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------

/// A fully-validated `in_acquire` spec.
#[derive(Clone, Debug, PartialEq)]
pub struct AcquireSpec {
    pub name: String,
    pub scope: Scope,
    /// `Some` when the `forward_args` key was present —
    /// including `Some(Value::Null)` for an explicit null.
    /// Only its presence decides whether a secondary tries
    /// to forward.
    pub forward_args: Option<json::Value>,
    pub forward_timeout_ms: u64,
}

fn spec_object(text: &str, what: &str) -> InResult<json::Value> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("{what} is not valid JSON: {e}")))?;
    if value.as_object().is_none() {
        return Err((ERR_PARSE, format!("{what} must be a JSON object")));
    }
    Ok(value)
}

fn reject_unknown_keys(value: &json::Value, allowed: &[&str], what: &str) -> InResult<()> {
    for key in value.keys() {
        if !allowed.contains(&key) {
            return Err((ERR_PARSE, format!("unknown {what} key \"{key}\"")));
        }
    }
    Ok(())
}

fn uint_field(value: &json::Value, key: &str, what: &str) -> InResult<Option<u64>> {
    match value.get(key) {
        None => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => Ok(Some(n as u64)),
            _ => Err((
                ERR_INVALID_ARG,
                format!(
                    "\"{key}\" in {what} must be a non-negative integer, got {}",
                    v.type_name()
                ),
            )),
        },
    }
}

/// Parse and validate an `in_acquire` spec:
///
///   {"name": "myapp", "scope": "local",
///    "forward_args": ["file.txt"], "forward_timeout_ms": 3000}
///
/// `name` is required and must match `[A-Za-z0-9._-]{1,64}`.
/// `scope` defaults to `"local"`. Unknown keys and
/// malformed JSON are `ERR_PARSE`; missing/wrong-typed/
/// out-of-alphabet values are `ERR_INVALID_ARG`.
pub fn parse_acquire_spec(text: &str) -> InResult<AcquireSpec> {
    let value = spec_object(text, "acquire spec")?;
    reject_unknown_keys(
        &value,
        &["name", "scope", "forward_args", "forward_timeout_ms"],
        "acquire spec",
    )?;
    let name = match value.get("name") {
        None => {
            return Err((
                ERR_INVALID_ARG,
                "acquire spec is missing required key \"name\"".to_string(),
            ))
        }
        Some(json::Value::Str(s)) => s.clone(),
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"name\" must be a string, got {}", other.type_name()),
            ))
        }
    };
    validate_name(&name)?;
    let scope = match value.get("scope") {
        None => Scope::Local,
        Some(json::Value::Str(s)) if s == "local" => Scope::Local,
        Some(json::Value::Str(s)) if s == "global" => Scope::Global,
        Some(json::Value::Str(s)) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"scope\" must be \"local\" or \"global\", got \"{s}\""),
            ))
        }
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"scope\" must be a string, got {}", other.type_name()),
            ))
        }
    };
    let forward_args = value.get("forward_args").cloned();
    let forward_timeout_ms = uint_field(&value, "forward_timeout_ms", "acquire spec")?
        .unwrap_or(DEFAULT_FORWARD_TIMEOUT_MS);
    Ok(AcquireSpec {
        name,
        scope,
        forward_args,
        forward_timeout_ms,
    })
}

/// A fully-validated `in_poll` spec.
#[derive(Clone, Debug, PartialEq)]
pub struct PollSpec {
    pub token: u64,
    /// Already capped at [`MAX_POLL_TIMEOUT_MS`].
    pub timeout_ms: u64,
}

/// Parse a poll spec: `{"token": n, "timeout_ms": n}`.
/// Both keys are required; a `timeout_ms` above the cap
/// is clamped to it (the documented cap semantics).
pub fn parse_poll_spec(text: &str) -> InResult<PollSpec> {
    let value = spec_object(text, "poll spec")?;
    reject_unknown_keys(&value, &["token", "timeout_ms"], "poll spec")?;
    let token = uint_field(&value, "token", "poll spec")?.ok_or((
        ERR_INVALID_ARG,
        "poll spec is missing required key \"token\"".to_string(),
    ))?;
    let timeout_ms = uint_field(&value, "timeout_ms", "poll spec")?
        .ok_or((
            ERR_INVALID_ARG,
            "poll spec is missing required key \"timeout_ms\"".to_string(),
        ))?
        .min(MAX_POLL_TIMEOUT_MS);
    Ok(PollSpec { token, timeout_ms })
}

/// Parse a release spec: `{"token": n}`.
pub fn parse_release_spec(text: &str) -> InResult<u64> {
    let value = spec_object(text, "release spec")?;
    reject_unknown_keys(&value, &["token"], "release spec")?;
    uint_field(&value, "token", "release spec")?.ok_or((
        ERR_INVALID_ARG,
        "release spec is missing required key \"token\"".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Result shaping
// ---------------------------------------------------------------------------

/// `{"role":"primary","token":n}`.
pub fn primary_result_json(token: u64) -> String {
    format!("{{\"role\":\"primary\",\"token\":{token}}}")
}

/// `{"role":"secondary","forwarded":true|false}`.
pub fn secondary_result_json(forwarded: bool) -> String {
    format!("{{\"role\":\"secondary\",\"forwarded\":{forwarded}}}")
}

/// `{"args": <value>}`, or `{"args": null}` on timeout.
pub fn poll_result_json(args: Option<&json::Value>) -> String {
    match args {
        Some(v) => {
            let mut out = String::with_capacity(16 + 64);
            out.push_str("{\"args\":");
            out.push_str(&v.to_json());
            out.push('}');
            out
        }
        None => "{\"args\":null}".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Mailbox — the primary's incoming queue
// ---------------------------------------------------------------------------

/// A source `poll_from` can draw forwarded messages from.
/// Exists so poll's timeout semantics are testable against
/// a fake channel with no OS objects involved.
pub trait MessageSource {
    /// Block up to `timeout_ms` for the next forwarded
    /// args value. `None` means the wait ended with no
    /// message (timeout, or the source was closed).
    fn next_message(&self, timeout_ms: u64) -> Option<json::Value>;
}

/// Poll semantics, pure: draw once from the source with
/// exactly the requested timeout; a message becomes
/// `{"args": ...}`, an empty wait becomes
/// `{"args": null}`.
pub fn poll_from<S: MessageSource>(source: &S, timeout_ms: u64) -> String {
    poll_result_json(source.next_message(timeout_ms).as_ref())
}

struct MailboxInner {
    queue: std::collections::VecDeque<json::Value>,
    closed: bool,
}

/// The primary's incoming queue: the listener thread
/// pushes forwarded args; `in_poll` pops them in arrival
/// order. Closing wakes every blocked waiter.
pub struct Mailbox {
    inner: std::sync::Mutex<MailboxInner>,
    changed: std::sync::Condvar,
}

impl Mailbox {
    pub fn new() -> Self {
        Mailbox {
            inner: std::sync::Mutex::new(MailboxInner {
                queue: std::collections::VecDeque::new(),
                closed: false,
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    /// Queue one forwarded message (listener thread).
    /// Messages pushed after close are dropped — the
    /// primary is going away.
    pub fn push(&self, args: json::Value) {
        {
            let mut g = lock(&self.inner);
            if g.closed {
                return;
            }
            g.queue.push_back(args);
        }
        self.changed.notify_all();
    }

    /// Wake all waiters; later waits return immediately
    /// (queued messages already inside are still drained
    /// first).
    pub fn close(&self) {
        {
            let mut g = lock(&self.inner);
            g.closed = true;
        }
        self.changed.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.inner).closed
    }

    pub fn queued(&self) -> usize {
        lock(&self.inner).queue.len()
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageSource for Mailbox {
    fn next_message(&self, timeout_ms: u64) -> Option<json::Value> {
        let mut g = lock(&self.inner);
        if let Some(v) = g.queue.pop_front() {
            return Some(v);
        }
        if g.closed || timeout_ms == 0 {
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return g.queue.pop_front();
            }
            let (guard, _) = self
                .changed
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
            if let Some(v) = g.queue.pop_front() {
                return Some(v);
            }
            if g.closed {
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Token registry — the primaries this process owns
// ---------------------------------------------------------------------------

/// One live primary: its mailbox plus the platform
/// resources (mutex handle, listener thread) that
/// `release` tears down.
pub struct Primary {
    pub mailbox: std::sync::Arc<Mailbox>,
    pub handle: platform::PrimaryHandle,
}

static REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<u64, std::sync::Arc<Primary>>>,
> = std::sync::OnceLock::new();
static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<u64, std::sync::Arc<Primary>>>
{
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Register a primary and mint its token. Tokens are
/// process-local, start at 1, and are never reused.
pub fn register_primary(mailbox: std::sync::Arc<Mailbox>, handle: platform::PrimaryHandle) -> u64 {
    let token = NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    lock(registry()).insert(token, std::sync::Arc::new(Primary { mailbox, handle }));
    token
}

pub fn get_primary(token: u64) -> Option<std::sync::Arc<Primary>> {
    lock(registry()).get(&token).cloned()
}

/// `in_poll`'s core: unknown token is `ERR_NOT_FOUND`;
/// otherwise one mailbox draw with the spec's timeout.
pub fn core_poll(token: u64, timeout_ms: u64) -> InResult<String> {
    let primary = get_primary(token).ok_or((
        ERR_NOT_FOUND,
        format!("no live primary with token {token} in this process"),
    ))?;
    Ok(poll_from(&*primary.mailbox, timeout_ms))
}

/// `in_release`'s core: remove the primary, close its
/// mailbox (waking blocked polls), then tear the platform
/// resources down (listener stopped and joined, mutex
/// closed). Unknown token is `ERR_NOT_FOUND`.
pub fn core_release(token: u64) -> InResult<()> {
    let primary = lock(registry()).remove(&token).ok_or((
        ERR_NOT_FOUND,
        format!("no live primary with token {token} in this process"),
    ))?;
    primary.mailbox.close();
    // The registry held the only strong reference unless a
    // poll is in flight on another thread; the platform
    // teardown needs the handle out of the Arc. When a
    // concurrent poll still holds a clone, the mailbox is
    // already closed (its poll returns {"args":null}
    // promptly), so spin briefly for the handle; a poll
    // never blocks longer than its own timeout and the
    // closed mailbox cuts that short immediately.
    let mut primary = primary;
    loop {
        match std::sync::Arc::try_unwrap(primary) {
            Ok(p) => {
                p.handle.release();
                return Ok(());
            }
            Err(arc) => {
                primary = arc;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The acquire state machine (pure; backends do the OS work)
// ---------------------------------------------------------------------------

/// What the mutex election decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutexAttempt {
    /// This process created the mutex: it is the primary.
    Created,
    /// The mutex already existed: another process is the
    /// primary.
    AlreadyExists,
}

/// The OS half of acquire, behind a trait so the state
/// machine is host-testable. Implementations: the real
/// Win32 backend in `platform`, a scripted fake in tests.
pub trait InstanceBackend {
    /// Create the election mutex. On `AlreadyExists` the
    /// backend must release its own handle to the existing
    /// mutex before returning (see `platform`) — a
    /// secondary that keeps a handle open would keep the
    /// name alive after the primary exits.
    fn create_mutex(&mut self, name: &str, scope: Scope) -> InResult<MutexAttempt>;

    /// Start the pipe listener for a fresh primary. Only
    /// called after `MutexAttempt::Created`; on success
    /// the returned handle owns the mutex and the listener.
    fn start_listener(
        &mut self,
        name: &str,
        mailbox: std::sync::Arc<Mailbox>,
    ) -> InResult<platform::PrimaryHandle>;

    /// Forward one frame to the running primary and wait
    /// for its ack, bounded by `timeout_ms`. True only
    /// when the ack arrived.
    fn forward(&mut self, name: &str, frame: &[u8], timeout_ms: u64) -> bool;
}

/// `in_acquire`'s core, platform-free:
///
/// 1. Run the mutex election.
/// 2. Created → start the listener, register the primary,
///    answer `{"role":"primary","token":n}`.
/// 3. AlreadyExists → when `forward_args` is present,
///    frame it and forward it (ack = truth); answer
///    `{"role":"secondary","forwarded":...}`. Without
///    `forward_args` nothing is forwarded and `forwarded`
///    is false.
pub fn acquire_with<B: InstanceBackend>(backend: &mut B, spec: &AcquireSpec) -> InResult<String> {
    match backend.create_mutex(&spec.name, spec.scope)? {
        MutexAttempt::Created => {
            let mailbox = std::sync::Arc::new(Mailbox::new());
            let handle = backend.start_listener(&spec.name, mailbox.clone())?;
            let token = register_primary(mailbox, handle);
            Ok(primary_result_json(token))
        }
        MutexAttempt::AlreadyExists => {
            let forwarded = match &spec.forward_args {
                None => false,
                Some(args) => {
                    let frame = encode_frame(forward_frame_json(args).as_bytes())?;
                    backend.forward(&spec.name, &frame, spec.forward_timeout_ms)
                }
            };
            Ok(secondary_result_json(forwarded))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    // -- Names --

    #[test]
    fn name_validation_rules() {
        assert!(validate_name("myapp").is_ok());
        assert!(validate_name("My.App_1-x").is_ok());
        assert!(validate_name(&"a".repeat(64)).is_ok());
        assert!(validate_name("..").is_ok()); // dots are in the alphabet
        for bad in [
            "",
            &"a".repeat(65),
            "my app",
            "a/b",
            "a\\b",
            "café",
            "a:b",
            "a|b",
        ] {
            let (code, _) = validate_name(bad).unwrap_err();
            assert_eq!(code, ERR_INVALID_ARG, "name {bad:?}");
        }
    }

    #[test]
    fn names_build_mutex_and_pipe_paths() {
        assert_eq!(mutex_name("app", Scope::Local), "Local\\app");
        assert_eq!(mutex_name("app", Scope::Global), "Global\\app");
        assert_eq!(pipe_name("app"), r"\\.\pipe\app-instance");
    }

    // -- Frame codec --

    #[test]
    fn frame_roundtrip_and_prefix() {
        let frame = encode_frame(b"hello").unwrap();
        assert_eq!(&frame[..4], &[5, 0, 0, 0]); // little-endian length
        assert_eq!(&frame[4..], b"hello");
        let mut dec = FrameDecoder::new();
        dec.feed(&frame);
        assert_eq!(dec.next_frame().unwrap(), Some(b"hello".to_vec()));
        assert_eq!(dec.next_frame().unwrap(), None);
        // An empty payload is a legal frame.
        let empty = encode_frame(b"").unwrap();
        dec.feed(&empty);
        assert_eq!(dec.next_frame().unwrap(), Some(Vec::new()));
    }

    #[test]
    fn frame_decoder_handles_split_and_batched_feeds() {
        let f1 = encode_frame(b"one").unwrap();
        let f2 = encode_frame(b"two!!").unwrap();
        let mut dec = FrameDecoder::new();
        // Byte-by-byte: nothing until the last byte lands.
        for (i, b) in f1.iter().enumerate() {
            dec.feed(&[*b]);
            let got = dec.next_frame().unwrap();
            if i + 1 < f1.len() {
                assert_eq!(got, None);
            } else {
                assert_eq!(got, Some(b"one".to_vec()));
            }
        }
        // Two frames in one feed come out in order.
        let mut both = f1.clone();
        both.extend_from_slice(&f2);
        dec.feed(&both);
        assert_eq!(dec.next_frame().unwrap(), Some(b"one".to_vec()));
        assert_eq!(dec.next_frame().unwrap(), Some(b"two!!".to_vec()));
        assert_eq!(dec.pending_bytes(), 0);
    }

    #[test]
    fn frame_size_limit_enforced_on_encode_and_decode() {
        let max = vec![7u8; MAX_FRAME_PAYLOAD];
        assert!(encode_frame(&max).is_ok());
        let over = vec![7u8; MAX_FRAME_PAYLOAD + 1];
        assert_eq!(encode_frame(&over).unwrap_err().0, ERR_INVALID_ARG);
        // A header declaring an oversized frame is rejected
        // as soon as the 4 header bytes arrive — no payload
        // needs to be sent.
        let mut dec = FrameDecoder::new();
        dec.feed(&u32::MAX.to_le_bytes());
        assert_eq!(dec.next_frame().unwrap_err().0, ERR_PLATFORM);
        let mut dec = FrameDecoder::new();
        dec.feed(&((MAX_FRAME_PAYLOAD as u32 + 1).to_le_bytes()));
        assert!(dec.next_frame().is_err());
        // A header declaring exactly the limit is accepted
        // (and simply incomplete without its payload).
        let mut dec = FrameDecoder::new();
        dec.feed(&(MAX_FRAME_PAYLOAD as u32).to_le_bytes());
        assert_eq!(dec.next_frame().unwrap(), None);
    }

    // -- Envelope --

    #[test]
    fn forward_envelope_roundtrip_and_ack() {
        let args = json::parse(r#"["file.txt",42,{"k":true},null]"#).unwrap();
        let text = forward_frame_json(&args);
        assert_eq!(text, r#"{"args":["file.txt",42,{"k":true},null]}"#);
        let back = parse_forward_frame(text.as_bytes()).unwrap();
        assert_eq!(back, args);
        // An explicit null args is a real message, not an error.
        assert_eq!(
            parse_forward_frame(b"{\"args\":null}").unwrap(),
            json::Value::Null
        );
        for bad in [
            &b"not json"[..],
            b"[1,2]",
            b"{\"other\":1}",
            b"{\"ack\":true}", // an ack is not a forward
        ] {
            assert_eq!(
                parse_forward_frame(bad).unwrap_err().0,
                ERR_PARSE,
                "{bad:?}"
            );
        }
        assert!(is_ack_frame(ACK_FRAME_JSON.as_bytes()));
        assert!(!is_ack_frame(b"{\"ack\":false}"));
        assert!(!is_ack_frame(b"{\"ack\":1}"));
        assert!(!is_ack_frame(b"{}"));
        assert!(!is_ack_frame(b"garbage"));
    }

    // -- Specs --

    #[test]
    fn acquire_spec_defaults_and_full() {
        let bare = parse_acquire_spec(r#"{"name":"app"}"#).unwrap();
        assert_eq!(bare.scope, Scope::Local);
        assert_eq!(bare.forward_args, None);
        assert_eq!(bare.forward_timeout_ms, DEFAULT_FORWARD_TIMEOUT_MS);

        let full = parse_acquire_spec(
            r#"{"name":"app","scope":"global","forward_args":{"x":1},"forward_timeout_ms":250}"#,
        )
        .unwrap();
        assert_eq!(full.scope, Scope::Global);
        assert_eq!(full.forward_timeout_ms, 250);
        assert_eq!(
            full.forward_args.unwrap().get("x").unwrap().as_i64(),
            Some(1)
        );

        // Explicit null forward_args counts as present.
        let null_args = parse_acquire_spec(r#"{"name":"app","forward_args":null}"#).unwrap();
        assert_eq!(null_args.forward_args, Some(json::Value::Null));
    }

    #[test]
    fn acquire_spec_errors() {
        let cases: &[(&str, i32)] = &[
            (r#"{"name":"#, ERR_PARSE),                   // malformed
            (r#"{"name":"a","name":"b"}"#, ERR_PARSE),    // duplicate key
            (r#"{"name":"a","bogus":1}"#, ERR_PARSE),     // unknown key
            (r#"["name"]"#, ERR_PARSE),                   // not an object
            (r#"{}"#, ERR_INVALID_ARG),                   // missing name
            (r#"{"name":""}"#, ERR_INVALID_ARG),          // empty name
            (r#"{"name":"has space"}"#, ERR_INVALID_ARG), // alphabet
            (r#"{"name":3}"#, ERR_INVALID_ARG),           // wrong type
            (r#"{"name":"a","scope":"both"}"#, ERR_INVALID_ARG),
            (r#"{"name":"a","scope":1}"#, ERR_INVALID_ARG),
            (r#"{"name":"a","forward_timeout_ms":-1}"#, ERR_INVALID_ARG),
            (r#"{"name":"a","forward_timeout_ms":1.5}"#, ERR_INVALID_ARG),
            (r#"{"name":"a","forward_timeout_ms":"9"}"#, ERR_INVALID_ARG),
        ];
        for (text, code) in cases {
            assert_eq!(
                parse_acquire_spec(text).unwrap_err().0,
                *code,
                "spec {text}"
            );
        }
    }

    #[test]
    fn poll_and_release_spec_rules() {
        let p = parse_poll_spec(r#"{"token":7,"timeout_ms":250}"#).unwrap();
        assert_eq!((p.token, p.timeout_ms), (7, 250));
        // The cap clamps; it is not an error.
        let capped = parse_poll_spec(r#"{"token":7,"timeout_ms":999999}"#).unwrap();
        assert_eq!(capped.timeout_ms, MAX_POLL_TIMEOUT_MS);
        let at_cap = parse_poll_spec(r#"{"token":7,"timeout_ms":60000}"#).unwrap();
        assert_eq!(at_cap.timeout_ms, MAX_POLL_TIMEOUT_MS);
        assert_eq!(
            parse_poll_spec(r#"{"timeout_ms":1}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_poll_spec(r#"{"token":1}"#).unwrap_err().0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_poll_spec(r#"{"token":-1,"timeout_ms":1}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
        assert_eq!(
            parse_poll_spec(r#"{"token":1,"timeout_ms":1,"x":0}"#)
                .unwrap_err()
                .0,
            ERR_PARSE
        );
        assert_eq!(parse_release_spec(r#"{"token":9}"#).unwrap(), 9);
        assert_eq!(parse_release_spec(r#"{}"#).unwrap_err().0, ERR_INVALID_ARG);
    }

    #[test]
    fn result_json_shapes() {
        assert_eq!(primary_result_json(3), r#"{"role":"primary","token":3}"#);
        assert_eq!(
            secondary_result_json(true),
            r#"{"role":"secondary","forwarded":true}"#
        );
        assert_eq!(
            secondary_result_json(false),
            r#"{"role":"secondary","forwarded":false}"#
        );
        assert_eq!(poll_result_json(None), r#"{"args":null}"#);
        let v = json::parse(r#"{"a":1}"#).unwrap();
        assert_eq!(poll_result_json(Some(&v)), r#"{"args":{"a":1}}"#);
    }

    // -- Poll semantics against a fake channel --

    struct FakeChannel {
        scripted: StdMutex<std::collections::VecDeque<Option<json::Value>>>,
        seen_timeouts: StdMutex<Vec<u64>>,
    }

    impl FakeChannel {
        fn new(script: Vec<Option<json::Value>>) -> Self {
            FakeChannel {
                scripted: StdMutex::new(script.into()),
                seen_timeouts: StdMutex::new(Vec::new()),
            }
        }
    }

    impl MessageSource for FakeChannel {
        fn next_message(&self, timeout_ms: u64) -> Option<json::Value> {
            self.seen_timeouts.lock().unwrap().push(timeout_ms);
            self.scripted.lock().unwrap().pop_front().unwrap_or(None)
        }
    }

    #[test]
    fn poll_semantics_against_fake_channel() {
        let args = json::parse(r#"["a.txt"]"#).unwrap();
        let ch = FakeChannel::new(vec![Some(args.clone()), None]);
        // A message becomes {"args": ...}...
        assert_eq!(poll_from(&ch, 1234), r#"{"args":["a.txt"]}"#);
        // ...an empty wait becomes {"args": null}...
        assert_eq!(poll_from(&ch, 1234), r#"{"args":null}"#);
        // ...and the exact requested timeout reached the
        // channel both times (poll neither pads nor trims).
        assert_eq!(*ch.seen_timeouts.lock().unwrap(), vec![1234, 1234]);
        // Timeout 0 is a legal non-blocking draw.
        let ch0 = FakeChannel::new(vec![]);
        assert_eq!(poll_from(&ch0, 0), r#"{"args":null}"#);
        assert_eq!(*ch0.seen_timeouts.lock().unwrap(), vec![0]);
    }

    // -- Mailbox (the real channel) --

    #[test]
    fn mailbox_fifo_wakeup_and_timeout() {
        let mb = Mailbox::new();
        mb.push(json::Value::I64(1));
        mb.push(json::Value::I64(2));
        assert_eq!(mb.next_message(0), Some(json::Value::I64(1)));
        assert_eq!(mb.next_message(0), Some(json::Value::I64(2)));

        // A push from another thread wakes a blocked wait.
        let shared = std::sync::Arc::new(Mailbox::new());
        let pusher = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                shared.push(json::Value::Str("late".into()));
            })
        };
        let got = shared.next_message(10_000);
        pusher.join().unwrap();
        assert_eq!(got, Some(json::Value::Str("late".into())));

        // An empty wait with a timeout really waits, then
        // returns None (bounded: never the full 10 s above,
        // and here not less than roughly the request).
        let empty = Mailbox::new();
        let t0 = Instant::now();
        assert_eq!(empty.next_message(120), None);
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(100), "waited {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
    }

    #[test]
    fn mailbox_close_wakes_waiters_and_drains_first() {
        let shared = std::sync::Arc::new(Mailbox::new());
        shared.push(json::Value::Bool(true));
        shared.close();
        // Queued messages drain even after close...
        assert_eq!(shared.next_message(0), Some(json::Value::Bool(true)));
        // ...then waits return at once instead of blocking.
        let t0 = Instant::now();
        assert_eq!(shared.next_message(60_000), None);
        assert!(t0.elapsed() < Duration::from_secs(5));
        // Pushes after close are dropped.
        shared.push(json::Value::Bool(false));
        assert_eq!(shared.queued(), 0);

        // A waiter blocked in another thread wakes on close.
        let shared2 = std::sync::Arc::new(Mailbox::new());
        let waiter = {
            let shared2 = shared2.clone();
            std::thread::spawn(move || shared2.next_message(60_000))
        };
        std::thread::sleep(Duration::from_millis(50));
        shared2.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    // -- The acquire state machine, against a fake backend --

    struct FakeBackend {
        attempt: MutexAttempt,
        forward_result: bool,
        forward_calls: Vec<(String, Vec<u8>, u64)>,
        listener_started_for: Option<String>,
        listener_mailbox: Option<std::sync::Arc<Mailbox>>,
        mutex_calls: Vec<(String, Scope)>,
    }

    impl FakeBackend {
        fn new(attempt: MutexAttempt) -> Self {
            FakeBackend {
                attempt,
                forward_result: true,
                forward_calls: Vec::new(),
                listener_started_for: None,
                listener_mailbox: None,
                mutex_calls: Vec::new(),
            }
        }
    }

    impl InstanceBackend for FakeBackend {
        fn create_mutex(&mut self, name: &str, scope: Scope) -> InResult<MutexAttempt> {
            self.mutex_calls.push((name.to_string(), scope));
            Ok(self.attempt)
        }

        fn start_listener(
            &mut self,
            name: &str,
            mailbox: std::sync::Arc<Mailbox>,
        ) -> InResult<platform::PrimaryHandle> {
            self.listener_started_for = Some(name.to_string());
            self.listener_mailbox = Some(mailbox);
            Ok(platform::PrimaryHandle::detached())
        }

        fn forward(&mut self, name: &str, frame: &[u8], timeout_ms: u64) -> bool {
            self.forward_calls
                .push((name.to_string(), frame.to_vec(), timeout_ms));
            self.forward_result
        }
    }

    fn acquire_spec(text: &str) -> AcquireSpec {
        parse_acquire_spec(text).unwrap()
    }

    #[test]
    fn state_machine_primary_path() {
        let mut be = FakeBackend::new(MutexAttempt::Created);
        let out = acquire_with(&mut be, &acquire_spec(r#"{"name":"sm-primary"}"#)).unwrap();
        let v = json::parse(&out).unwrap();
        assert_eq!(v.get("role").unwrap().as_str(), Some("primary"));
        let token = v.get("token").unwrap().as_i64().unwrap() as u64;
        assert!(token >= 1);
        // The listener started for the same name, the
        // election ran in the default scope, and nothing
        // was forwarded.
        assert_eq!(be.listener_started_for.as_deref(), Some("sm-primary"));
        assert_eq!(
            be.mutex_calls,
            vec![("sm-primary".to_string(), Scope::Local)]
        );
        assert!(be.forward_calls.is_empty());
        // The registered primary is really reachable: push
        // through the backend's mailbox, poll it back out.
        be.listener_mailbox
            .as_ref()
            .unwrap()
            .push(json::Value::Str("hi".into()));
        assert_eq!(core_poll(token, 0).unwrap(), r#"{"args":"hi"}"#);
        core_release(token).unwrap();
        assert_eq!(core_poll(token, 0).unwrap_err().0, ERR_NOT_FOUND);
    }

    #[test]
    fn state_machine_secondary_paths() {
        // With args + ack → forwarded:true, frame well-formed.
        let mut be = FakeBackend::new(MutexAttempt::AlreadyExists);
        let out = acquire_with(
            &mut be,
            &acquire_spec(r#"{"name":"sm-sec","forward_args":["f.txt"],"forward_timeout_ms":777}"#),
        )
        .unwrap();
        assert_eq!(out, r#"{"role":"secondary","forwarded":true}"#);
        assert_eq!(be.forward_calls.len(), 1);
        let (name, frame, timeout) = &be.forward_calls[0];
        assert_eq!(name, "sm-sec");
        assert_eq!(*timeout, 777);
        let mut dec = FrameDecoder::new();
        dec.feed(frame);
        let payload = dec.next_frame().unwrap().unwrap();
        assert_eq!(
            parse_forward_frame(&payload).unwrap(),
            json::parse(r#"["f.txt"]"#).unwrap()
        );
        assert!(be.listener_started_for.is_none());

        // With args, no ack → forwarded:false (honest).
        let mut be = FakeBackend::new(MutexAttempt::AlreadyExists);
        be.forward_result = false;
        let out = acquire_with(
            &mut be,
            &acquire_spec(r#"{"name":"sm-sec","forward_args":1}"#),
        )
        .unwrap();
        assert_eq!(out, r#"{"role":"secondary","forwarded":false}"#);
        assert_eq!(be.forward_calls.len(), 1);
        // Default forward timeout reached the backend.
        assert_eq!(be.forward_calls[0].2, DEFAULT_FORWARD_TIMEOUT_MS);

        // Without args → forward never attempted.
        let mut be = FakeBackend::new(MutexAttempt::AlreadyExists);
        let out = acquire_with(&mut be, &acquire_spec(r#"{"name":"sm-sec"}"#)).unwrap();
        assert_eq!(out, r#"{"role":"secondary","forwarded":false}"#);
        assert!(be.forward_calls.is_empty());
    }

    #[test]
    fn state_machine_backend_errors_propagate() {
        struct FailingBackend;
        impl InstanceBackend for FailingBackend {
            fn create_mutex(&mut self, _name: &str, _scope: Scope) -> InResult<MutexAttempt> {
                Err((ERR_NOT_SUPPORTED, "no backend here".to_string()))
            }
            fn start_listener(
                &mut self,
                _name: &str,
                _mailbox: std::sync::Arc<Mailbox>,
            ) -> InResult<platform::PrimaryHandle> {
                unreachable!()
            }
            fn forward(&mut self, _name: &str, _frame: &[u8], _timeout_ms: u64) -> bool {
                unreachable!()
            }
        }
        let mut be = FailingBackend;
        let (code, _) = acquire_with(&mut be, &acquire_spec(r#"{"name":"sm-fail"}"#)).unwrap_err();
        assert_eq!(code, ERR_NOT_SUPPORTED);
    }

    #[test]
    fn tokens_are_unique_and_increasing() {
        let a = register_primary(
            std::sync::Arc::new(Mailbox::new()),
            platform::PrimaryHandle::detached(),
        );
        let b = register_primary(
            std::sync::Arc::new(Mailbox::new()),
            platform::PrimaryHandle::detached(),
        );
        assert!(b > a, "tokens {a} then {b}");
        core_release(a).unwrap();
        core_release(b).unwrap();
        // Releasing twice is NOT_FOUND.
        assert_eq!(core_release(a).unwrap_err().0, ERR_NOT_FOUND);
    }
}
