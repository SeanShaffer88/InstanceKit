/* InstanceKit.dll — single-instance enforcement with
 * argument forwarding, behind one small C ABI.
 *
 * The problem this solves: every desktop app hand-rolled
 * the same trick — create a named mutex; if it already
 * exists, a second copy of the app is starting, so hand
 * the running copy whatever the user asked for (the file
 * they double-clicked, the link they clicked) and exit —
 * each with its own ad-hoc IPC, framing and guesswork
 * about whether the primary actually received anything.
 * InstanceKit is the one implementation:
 *
 *   char *r = in_acquire(
 *       "{\"name\":\"myapp\",\"forward_args\":[\"report.txt\"]}");
 *   // r == {"role":"secondary","forwarded":true}  -> exit;
 *   //      the running copy got the file.
 *   // r == {"role":"primary","token":1}           -> run;
 *   //      poll for what later copies forward:
 *   char *m = in_poll("{\"token\":1,\"timeout_ms\":250}");
 *   // m == {"args":["report.txt"]} or {"args":null}
 *   ...
 *   in_release("{\"token\":1}");
 *
 * THE ELECTION. in_acquire creates the named mutex
 * Local\<name> (scope "local", the default) or
 * Global\<name> (scope "global"). The creator is the
 * PRIMARY; a caller that finds the mutex already existing
 * is a SECONDARY. The kernel guarantees exactly one
 * creator, so there is no race. "local" means one primary
 * per login session; "global" means one per machine
 * (creating Global\ objects can require a privilege for
 * services — a failure is IN_ERR_ACCESS_DENIED / a
 * platform error, never a silent fallback to local).
 *
 * FORWARDING, ACKNOWLEDGED. When a secondary's spec
 * carries forward_args (any JSON value, including null —
 * the key's PRESENCE is what counts), it connects to the
 * primary's named pipe \\.\pipe\<name>-instance, sends one
 * length-prefixed frame {"args": <forward_args>}, and
 * waits for the primary's {"ack":true} frame. The wait is
 * bounded by forward_timeout_ms (default 3000), and the
 * connect retries while the pipe is still coming up.
 * "forwarded" is true ONLY when the ack arrived: a dead
 * or dying primary, a malformed exchange, or a timeout
 * all report false. Forwarding never fails the acquire
 * itself — a secondary always learns it is a secondary.
 * A secondary without forward_args connects to nothing
 * and reports forwarded:false.
 *
 * WIRE FORMAT. One frame per connection: a 4-byte
 * little-endian length prefix followed by a UTF-8 JSON
 * payload. Payloads are capped at 1 MiB (1,048,576
 * bytes); an oversized payload is rejected at send time
 * (in_acquire fails with IN_ERR_INVALID_ARG) and an
 * oversized declared frame is dropped at receive time
 * (no ack). The primary serves one frame per connection,
 * queues the args, acks, and disconnects.
 *
 * POLLING. The primary's forwarded messages queue in
 * arrival order. in_poll({"token":n,"timeout_ms":ms})
 * returns the next one as {"args": <the exact JSON value
 * the secondary sent>}, or {"args": null} when the wait
 * elapses with nothing queued. timeout_ms is required and
 * capped at 60000 — larger values are clamped, not an
 * error. timeout_ms 0 is a non-blocking probe. Each
 * message is delivered to exactly one poll; concurrent
 * polls on one token compete for the queue. Polling an
 * unknown or released token fails with IN_ERR_NOT_FOUND.
 *
 * RELEASE. in_release({"token":n}) stops the listener
 * thread and JOINS it, closes the pipe and mutex handles,
 * and wakes any blocked poll (which returns
 * {"args":null}). Once release returns, the name is free:
 * the next in_acquire for it becomes the new primary.
 * Tokens are process-local, start at 1, and are never
 * reused. A primary that exits WITHOUT releasing loses
 * nothing for the system — the OS closes its handles with
 * the process — but a long-lived host should always
 * release, exactly as it would close any handle. A
 * secondary has no token and nothing to release.
 *
 * NAMES. "name" must match [A-Za-z0-9._-]{1,64}. The
 * restricted alphabet is what makes the name safe to
 * embed verbatim in a mutex name and a pipe path.
 *
 * ERRORS. in_acquire / in_poll return NULL on failure and
 * set the thread-local last error (in_last_error /
 * in_last_error_code). in_release returns IN_OK (0) or an
 * IN_ERR_* code directly. Malformed JSON and unknown keys
 * are IN_ERR_PARSE; missing, wrong-typed or out-of-range
 * values are IN_ERR_INVALID_ARG; duplicate keys in one
 * object are a parse error. Off Windows, in_acquire fails
 * with IN_ERR_NOT_SUPPORTED.
 *
 * STRINGS. All strings are UTF-8 and NUL-terminated.
 * char* results are caller-owned: free with
 * in_free_string(). in_version / in_last_error return
 * borrowed strings — do NOT free them.
 */
#ifndef INSTANCEKIT_H
#define INSTANCEKIT_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#if defined(_WIN32) && defined(IN_BUILD)
#define IN_API __declspec(dllexport)
#elif defined(_WIN32)
#define IN_API __declspec(dllimport)
#else
#define IN_API
#endif
#define IN_OK 0
#define IN_ERR_INVALID_ARG 1
#define IN_ERR_NOT_SUPPORTED 2
#define IN_ERR_NOT_FOUND 3
#define IN_ERR_ACCESS_DENIED 4
#define IN_ERR_PLATFORM 5
#define IN_ERR_INTERNAL 6
#define IN_ERR_PARSE 7

/* Borrowed string: the DLL version, "0.1.0". */
IN_API const char *in_version(void);
/* Borrowed string: the calling thread's last error
 * message ("" when the last call succeeded). */
IN_API const char *in_last_error(void);
/* The calling thread's last error code (IN_OK or an
 * IN_ERR_* value). */
IN_API int in_last_error_code(void);
/* Free a caller-owned string returned by in_acquire /
 * in_poll. NULL is a no-op. */
IN_API void in_free_string(char *p);

/* Run the election (see the header docs). spec_json:
 *   {"name": "...",                  (required)
 *    "scope": "local"|"global",     (default "local")
 *    "forward_args": <any JSON>,    (optional; presence
 *                                    triggers forwarding)
 *    "forward_timeout_ms": n}       (default 3000)
 * Returns caller-owned JSON:
 *   {"role":"primary","token":n}
 *   {"role":"secondary","forwarded":true|false}
 * or NULL on failure (see in_last_error). */
IN_API char *in_acquire(const char *spec_json);

/* Take the next forwarded message for a primary.
 * spec_json: {"token": n, "timeout_ms": n} — both
 * required; timeout_ms is capped at 60000. Returns
 * caller-owned {"args": <value>} or {"args": null} on
 * timeout, or NULL on failure (IN_ERR_NOT_FOUND for an
 * unknown/released token). */
IN_API char *in_poll(const char *spec_json);

/* Release a primary: stop + join its listener, close its
 * pipe and mutex handles, free its name. spec_json:
 * {"token": n}. Returns IN_OK or an IN_ERR_* code
 * (IN_ERR_NOT_FOUND for an unknown/released token). */
IN_API int in_release(const char *spec_json);

#ifdef __cplusplus
}
#endif
#endif
