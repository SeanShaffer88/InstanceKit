# InstanceKit Roadmap

## v0.1.0 (current)
Single-instance enforcement behind a C ABI: the named-mutex election (`Local\` / `Global\`), acknowledged argument forwarding over a named pipe (one length-prefixed JSON frame, 1 MiB cap, ack round-trip bounded by `forward_timeout_ms`), an arrival-ordered poll queue on the primary with a capped blocking timeout, and a release that stops and joins the listener and frees the name. The state machine, framing, envelope, and parsers are pure and host-tested; the Win32 backend is one trait implementation in `platform.rs`.

## Next, in order
1. **Field verification pass**: run the Windows integration scenario on a real machine (two real processes, not two calls in one test process; a `global`-scope pair across sessions; a primary killed mid-forward) and record the results in `docs/PARITY.md`. Everything else waits on knowing the v0.1 transport is honest under real process lifetimes.
2. **Activation payload helpers**: typed builders/parsers for the two payload shapes every app actually forwards — "open these paths" and "handle this URL/protocol link" — so hosts stop hand-shaping the same JSON arrays. Pure helpers only; the wire format does not change.
3. **A `status` call**: `in_status({"token":n})` reporting queued count, total forwarded, total acked-timeouts observed by secondaries is not knowable — so: queued count, listener up/down, name and scope. For hosts that want to explain a "second copy didn't wake us" report without guessing.
4. **Wait-for-primary-exit**: a secondary (or an updater) may want to block until the current primary releases the name — `in_wait({"name":...,"timeout_ms":...})` over a wait on the mutex handle. Needs care: the mutex is never owned, so this is a handle-lifetime wait, not an ownership wait, and the semantics must be written before the code.

## Explicitly not planned
- Election by file lock, registry flag, or TCP port: the kernel mutex is the only mechanism with exactly-one-creator semantics and automatic cleanup on process death. Anything else reintroduces the races this kit exists to remove.
- Message replies / RPC: forwarding is one-way by design (args in, ack of *queuing* back). Request-response belongs to a real IPC library; bolting it on here would turn a 15-minute adoption into a framework.
- Cross-machine instances: "single instance" is a per-machine (or per-session) statement. A network election is a different product.
- Owning the mutex (waiting on it for mutual exclusion between primaries): the handle is an existence token only. Taking ownership invites the abandoned-mutex states the current design avoids by construction.
