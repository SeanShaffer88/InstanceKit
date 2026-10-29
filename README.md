# InstanceKit

## TL;DR

One single-instance helper for every app, behind a tiny zero-dependency DLL: the first copy of your app to call `in_acquire` becomes the primary and runs; every later copy learns it is a secondary, hands the primary its arguments over a named pipe (with an acknowledgement, so "forwarded" never means "probably"), and can exit. No more per-app mutex tricks, per-app pipe protocols, or second copies opening a second window on the same document.

```c
#include "instancekit.h"

int main(int argc, char **argv) {
    /* Build forward_args from argv (any JSON value). */
    char *r = in_acquire(
        "{\"name\":\"myapp\",\"forward_args\":[\"report.txt\"]}");
    if (!r) { /* in_last_error() says why */ return 1; }

    if (strstr(r, "\"secondary\"")) {
        /* {"role":"secondary","forwarded":true} — the
         * running copy has the file. Exit cleanly. */
        in_free_string(r);
        return 0;
    }
    /* {"role":"primary","token":1} — we are the app. */
    in_free_string(r);
    for (;;) {
        char *m = in_poll("{\"token\":1,\"timeout_ms\":250}");
        /* {"args":["report.txt"]} — open what arrived;
         * {"args":null} — nothing this tick. */
        in_free_string(m);
        /* ... run the app ... */
    }
    in_release("{\"token\":1}");
}
```

## Why this exists

Every desktop app needs the same answer to "the user double-clicked a second copy": don't start over — wake the running one and give it the new request. In this family that logic was hand-rolled per app, and the hand-rolled versions each invented their own IPC and their own answer to whether the primary actually received anything (WinDevKit pain list: the app-plumbing row — AppHost gives *new* apps a skeleton; InstanceKit is the drop-in library an *existing* app can call). InstanceKit makes it three calls:

- **The mutex is the election.** `in_acquire` creates `Local\<name>` (default) or `Global\<name>`. Exactly one process can create it, so exactly one primary exists — no race, no negotiation. Local = one per login session; global = one per machine.
- **Forwarding is acknowledged.** A secondary with `forward_args` sends one length-prefixed JSON frame (`{"args": ...}`) over `\\.\pipe\<name>-instance` and waits for the primary's ack frame, bounded by `forward_timeout_ms` (default 3000). `forwarded: true` means the primary queued the message — not that a write call returned. A dead primary, a timeout, or a malformed exchange all report `false`, and forwarding never fails the acquire itself: a secondary always learns it is a secondary.
- **The primary polls.** Messages queue in arrival order; `in_poll` returns the next as `{"args": ...}` — the exact JSON value the secondary sent — or `{"args": null}` when its timeout (required, capped at 60 000 ms) elapses.
- **Release is total.** `in_release` stops and joins the listener thread, closes the pipe and mutex handles, and wakes any blocked poll. The name is then free: the next starter becomes the new primary.

## The wire format

One frame per connection: a 4-byte little-endian length prefix + a UTF-8 JSON payload, capped at 1 MiB. Oversized payloads are rejected at send time (`IN_ERR_INVALID_ARG`) and dropped un-acked at receive time. The restricted name alphabet (`[A-Za-z0-9._-]{1,64}`) is what makes a name safe to embed verbatim in a mutex name and a pipe path.

## The API in one screen

| Area | Calls |
|---|---|
| Election + forwarding | `in_acquire` (spec JSON in, role JSON out) |
| Receiving | `in_poll` (`{"token":n,"timeout_ms":n}` → `{"args":...}` / `{"args":null}`) |
| Teardown | `in_release` (`{"token":n}` → `IN_OK` / `IN_ERR_*`) |
| Errors | Integer `IN_*` codes + thread-local `in_last_error()` / `in_last_error_code()` |
| Strings | `in_version` (borrowed), `in_free_string` (for caller-owned results) |

Error discipline follows the family convention: malformed JSON and unknown keys are `IN_ERR_PARSE`; missing, wrong-typed or out-of-alphabet values are `IN_ERR_INVALID_ARG`; duplicate keys in one object are a parse error. Tokens are process-local, start at 1, and are never reused; polling or releasing an unknown token is `IN_ERR_NOT_FOUND`.

## Honest caveats

- **Windows-only at runtime.** The election is a kernel mutex and the transport is a named pipe; off Windows `in_acquire` fails with `IN_ERR_NOT_SUPPORTED`. (The state machine, framing, and parsers are platform-free and tested on Linux.)
- **`global` scope can need a privilege.** Creating `Global\` objects from a service context can require SeCreateGlobalPrivilege; failure surfaces as an error, never as a silent fallback to local scope.
- **A primary that never releases still frees the name at exit** — the OS closes its handles with the process — but while it lives, the name stays claimed even if the app has stopped polling. Release is the host's job, like closing any handle.
- **`forwarded` proves queuing, not handling.** The ack means the args are in the primary's queue. Whether the primary *acts* on them is the primary's business; a primary that never polls will ack and ignore. Poll promptly.
- **One ack per connection, one frame per connection.** A secondary that has three things to say sends one JSON array, not three connections.

## Verification status

27 tests on the host: name validation and namespace building, the frame codec (roundtrip, split/batched feeds, the 1 MiB limit enforced on encode *and* on the decode header), the forward envelope and ack recognition, spec parsing for all three calls (defaults, the poll-timeout cap clamping, unknown/duplicate keys, wrong types), poll semantics against a scripted fake channel (exact timeout pass-through), the real mailbox (FIFO, wakeup on push, close waking waiters, drain-after-close), the full acquire state machine against a fake backend (primary registers and polls; secondary with ack / without ack / without args; backend errors propagate), token uniqueness, and FFI end-to-end cycles. The Windows-only integration test (primary → secondary forward → poll → release → re-acquire, through the real mutex and pipe) compiles under the Windows target in the gate and runs in Windows CI; **its runtime behaviour is Unverified until a field pass on a real Windows machine** — see `docs/PARITY.md`.

Building: `cargo build --release` → `instance_kit.dll` / `libinstance_kit.so`; tests: `cargo test --locked` (full WinDevKit gate: `~/workspace/windevkit/tools/gate.sh`).

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
