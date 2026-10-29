# Parity / Capability Matrix — InstanceKit

Updated: 2026-10-28 · Overall readiness: v0.1.0 complete locally — full WinDevKit gate green (see QUALITY.md); Windows runtime evidence is CI's real MSVC build plus a future on-machine field pass (Partially Verified). The pure core (state machine, framing, envelope, specs, mailbox, registry) is unit-tested on Linux; the Win32 backend's runtime behaviour is Unverified until a field test on a real Windows machine.

A row may say **Complete** only with a passing test or field result named in Evidence.

| Area | Target | State | Evidence (test / corpus) | Gap |
|---|---|---|---|---|
| Name validation | `[A-Za-z0-9._-]{1,64}`; everything else is `ERR_INVALID_ARG` | Complete | `name_validation_rules` (boundaries at 1/64/65 chars, separators, whitespace, non-ASCII) | — |
| Namespaces | `Local\<name>` / `Global\<name>` mutex names; `\\.\pipe\<name>-instance` pipe path | Complete | `names_build_mutex_and_pipe_paths` | — |
| Frame codec | 4-byte LE length prefix; 1 MiB payload cap | Complete | `frame_roundtrip_and_prefix`, `frame_decoder_handles_split_and_batched_feeds` (byte-at-a-time and batched feeds) | — |
| Frame cap | Encode rejects > 1 MiB; decoder rejects an oversized DECLARED length at header completion | Complete | `frame_size_limit_enforced_on_encode_and_decode` (incl. `u32::MAX` header, exact-limit header accepted) | — |
| Envelope | `{"args": ...}` roundtrip incl. explicit null; ack is exactly `{"ack":true}` | Complete | `forward_envelope_roundtrip_and_ack` (malformed / non-object / missing-key frames are `ERR_PARSE`) | — |
| Acquire spec | Defaults (local scope, 3000 ms forward timeout); `forward_args` presence incl. explicit null; unknown/duplicate keys = parse error; wrong types = invalid arg | Complete | `acquire_spec_defaults_and_full`, `acquire_spec_errors` | — |
| Poll / release specs | Both token and timeout required for poll; timeout clamped (not errored) at 60 000 | Complete | `poll_and_release_spec_rules` | — |
| Result shapes | `{"role":"primary","token":n}`, `{"role":"secondary","forwarded":b}`, `{"args":v}` / `{"args":null}` | Complete | `result_json_shapes` | — |
| Poll semantics | The exact requested timeout reaches the channel; a message becomes `{"args":...}`, an empty wait `{"args":null}` | Complete | `poll_semantics_against_fake_channel` (scripted fake channel records requested timeouts) | — |
| Mailbox | FIFO order; push wakes a blocked wait; close wakes waiters; queued messages drain after close; pushes after close dropped | Complete | `mailbox_fifo_wakeup_and_timeout` (incl. a real ~120 ms empty wait), `mailbox_close_wakes_waiters_and_drains_first` | — |
| Acquire state machine | Created → listener + registry + primary result; AlreadyExists → forward only when args present; ack decides `forwarded`; backend errors propagate | Complete | `state_machine_primary_path`, `state_machine_secondary_paths`, `state_machine_backend_errors_propagate` (scripted fake backend) | — |
| Registry / tokens | Tokens unique, increasing, never reused; release removes; double release is `ERR_NOT_FOUND` | Complete | `tokens_are_unique_and_increasing`, `state_machine_primary_path` | — |
| FFI surface | Every header export exercised; null/malformed inputs return the documented codes | Complete | `ffi_version_and_free_null`, `ffi_acquire_spec_errors`, `ffi_poll_and_release_unknown_token`, `ffi_full_cycle_against_registered_primary` | Windows MSVC link verified by CI, not locally |
| Off-Windows contract | `in_acquire` fails `ERR_NOT_SUPPORTED`; poll/release on unknown tokens fail `ERR_NOT_FOUND` | Complete | `ffi_acquire_not_supported_off_windows`, `ffi_poll_and_release_unknown_token` | — |
| Win32 election | `CreateMutexW` decides primary/secondary; secondary closes its handle immediately | Partially Verified | Code-reviewed; compiles + clippies clean for `x86_64-pc-windows-msvc` in the local gate; integration test `primary_secondary_forward_poll_release` written | Runtime Unverified — no Windows machine has run it yet; CI's MSVC build is the link gate, the field pass is the behaviour gate |
| Win32 forwarding | Overlapped pipe listener (one frame per connection, ack after queueing); client connect retries until the forward deadline; ack read bounded by the same deadline | Partially Verified | Same as above — parser/envelope halves are the host-tested pure code; the overlapped machinery is target-checked | Runtime Unverified — ack timing, pipe-busy retries, and stop-event joins have never executed on Windows |
| Release at runtime | Stop event wakes every listener wait; thread joined; mutex close frees the name for a new primary | Partially Verified | Integration test covers release → re-acquire as primary (compile-verified only) | Runtime Unverified until the field pass |
| Global scope | `Global\` election across sessions | Partially Verified | Mutex-name construction is unit-tested; scope parse is unit-tested | Cross-session behaviour (and the privilege failure path) Unverified — needs two real sessions on a Windows machine |
