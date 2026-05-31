# M0 Spike Report

## Scope

This report covers the M0 smoke path requested by the product and architecture documents:

1. start a mock daemon through `la-ipc`;
2. spawn a mock `claude` backend through `la-pty`;
3. attach with JSON-RPC;
4. write a prompt through the PTY;
5. receive the backend reply;
6. detach without killing the backend process.

The executable evidence is `integration/m0-smoke/tests/m0_smoke.rs`, run by the workspace test suite and the GitHub Actions matrix in `.github/workflows/ci.yml`.

## Backend JSON mode probe results

PRD §6.5 asks whether agent backends can provide structured JSON-mode behavior. M0 does not depend on real backend credentials, so the smoke harness uses a deterministic mock `claude` process with the same operational shape the daemon needs from a backend: startup banner, stdin prompt input, stdout reply streaming, and long-lived process ownership.

| Backend | Probe command in M0 | Result | M0 implication |
| --- | --- | --- | --- |
| `claude` | mock CLI spawned behind a PTY | PASS: prompt bytes written through `la-pty` produce a streamed textual reply | The daemon/client protocol can carry backend output without assuming real Claude auth or JSON support. |
| `codex` | not executed in M0 | NOT PROBED | Keep adapter-specific JSON schema detection behind future adapter probes. |
| `opencode` | not executed in M0 | NOT PROBED | Keep adapter-specific JSON schema detection behind future adapter probes. |

Verified assumption: LazyAgents should not make the core PTY/session path depend on native JSON output from any backend. Backend JSON support should be discovered per adapter and exposed as a capability, not required by the transport.

## `portable-pty` / ConPTY behavior and known risks

Current implemented evidence comes from `la-pty` unit tests plus the M0 smoke test:

| Topic | Unix/macOS evidence | Windows ConPTY expectation / risk |
| --- | --- | --- |
| Spawn/read/write | `la-pty` tests spawn `echo`/`cat`; M0 smoke spawns the mock `claude` shell loop | ConPTY should expose the same master read/write contract, but may emit extra console control sequences. Consumers must parse with a tolerant terminal parser. |
| Resize | `la-pty` calls `MasterPty::resize` and asserts no error | ConPTY resize maps to `ResizePseudoConsole`; Windows apps do not receive `SIGWINCH`, so adapters must not rely on POSIX signal semantics. |
| Signals | `Signal::Interrupt`, `Terminate`, and `Kill` are mapped per platform | Windows uses `GenerateConsoleCtrlEvent` for Ctrl-C/Break and `TerminateProcess` for hard kill. Hosted CI showed Ctrl-C delivery can succeed at the API layer without terminating a `ping` process behind ConPTY; shutdown paths need timeout + hard-kill fallback. |
| EOF | `la-pty` drops the slave handle after spawn so reader EOF is observable on Unix | GitHub-hosted Windows showed ConPTY reader EOF is not prompt for a short-lived `cmd /C echo` child, even after output is visible. The daemon must not depend on EOF as the only liveness signal. |
| Detach semantics | M0 smoke detaches the client, then writes an internal probe to the same PTY and receives a reply | This validates daemon ownership: client detach must remove only the subscription/input lease, not terminate the child. |

Known risk: `portable-pty` normalizes the API but not all terminal byte streams. Windows ConPTY can inject cursor queries, mode changes, and OSC/control bytes; renderer and replay layers must be byte-preserving and tolerant.

## Validated / refuted assumptions

| Assumption | Status | Evidence |
| --- | --- | --- |
| A length-prefixed JSON-RPC transport is enough for the M0 daemon/client protocol | Validated | `la-ipc::FramedJson` round-trips `RpcRequest`/`RpcResponse`; M0 smoke uses it end-to-end. |
| The session process can outlive a client detach | Validated | `sessions.detach` is followed by `sessions.probe_alive`, which writes to the same PTY and receives `mock-claude reply: post-detach`. |
| The core smoke path requires real Claude credentials | Refuted | The mock backend proves transport/session semantics without external auth or network dependencies. |
| Backend JSON mode should be a hard dependency of session attach/write | Refuted | The smoke passes with plain streamed bytes. JSON-mode support belongs in adapter capabilities. |

## CI evidence

The workspace now includes a GitHub Actions matrix for:

- `ubuntu-latest`
- `macos-latest`
- `windows-latest`

Each job runs `cargo test --workspace`, covering `la-ipc`, `la-pty`, and `integration/m0-smoke`. The matrix is the required production evidence source for ConPTY behavior; local Linux-only execution is not sufficient to certify Windows readiness. At the time this report was written, the matrix workflow had been added to the repository but had not yet produced a hosted GitHub Actions run in this workspace.
