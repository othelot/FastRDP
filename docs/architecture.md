# Architecture

## Components

| Component | Runs as | Role |
|---|---|---|
| `fastrdp_provider.dll` (`crates/provider`) | inside TermService (svchost, `NT SERVICE\TermService`) | Custom RDS protocol provider: `IWRdsProtocolManager` → `IWRdsProtocolListener` → `IWRdsProtocolConnection`. Turns a client connection into a Windows remote session. |
| `FastRdpIdd` (`drivers/FastRdpIdd`) | WUDFHost, one device per remote session | Remote Indirect Display Driver (IddCx 1.10). The session's monitor: receives the desktop frames and the hardware cursor. |
| `fastrdp-server` (`crates/server`) | Windows service `FastRdpServer`, LOCAL SERVICE | Network endpoint (QUIC). Relays authentication to the provider, reads frames from the driver, encodes, sends video; relays input, pointer, display changes and virtual channels. |
| `fastrdp-agent` (`crates/agent`) | Task Scheduler, as the session user, in every session | Starts `rdpclip.exe` in FastRDP sessions; SendInput fallback when the session input devices are not available. |
| `fastrdp-client` (`crates/client`) | user | iced GUI: video (GPU YUV→RGB shader), input, clipboard, certificate trust, settings. |

Shared crates: `protocol` (messages + framing), `transport` (video datagrams + FEC),
`frame-shm` (driver ↔ server shared memory), `encoder` (H.264 with fallback).

## Connection sequence

1. The client opens a QUIC connection to UDP 33890 (ALPN `fastrdp/1`). The server certificate is
   self-signed; the client accepts it only if its SHA-256 matches the remembered or confirmed
   fingerprint. The TLS verifier aborts *before* any credentials are sent.
2. On the bidirectional control stream: `HELLO` (client name, desktop size, experience flags),
   `AUTH` (user, domain, password), `DISPLAY` (size, refresh, DPI scale).
3. The server opens `\\.\pipe\FastRDP.Provider.FastRDP` (pipe DACL: SYSTEM, LOCAL SERVICE,
   Administrators, TermService; remote pipe clients rejected) and forwards `HELLO` / `AUTH`.
4. The provider calls `IWRdsProtocolListenerCallback::OnConnected`. TermService then drives the
   connection: `GetClientData` (experience flags, desktop size), `GetUserCredentials`
   (the password is handed over and wiped), `GetInputHandles`, `EnableWddmIdd` / `GetHardwareId` (the IDD
   hardware id), `CreateVirtualChannel`, `NotifySessionId`, `ConnectNotify`, `IsUserAllowedToLogon`,
   `LogonNotify`, … Each step is reported to the server as an `EVENT`.
5. Windows creates (or reconnects) the session, loads `FastRdpIdd` for it and logs the user on.
   Windows 11 Pro allows one active user: like RDP, connecting takes the session over from the
   console (or from another client), and the other side is disconnected.
6. The server learns the session id, opens the session's shared memory and starts streaming.

Disconnecting the client closes the pipe; the provider reports a broken connection and the session
becomes *Disconnected* (it keeps running and can be reconnected).

## Session arbitration

Which session a connection gets is decided by termsrv and lsm, and both check that the session is
compatible with the connection's terminal type (`CUtils::IsTerminalCompatibleToSession`; lsm again
in `CSessionReconnectController::TryApplyTerminalToSession`, failing with
ERROR_CTX_SESSION_INCOMPATIBLE). The terminal type of a protocol-provider connection is "RDP" only
if the listener's `LoadableProtocol_Object` is one of two CLSIDs built into termsrv
(`IsRDPProtocol`): rdpcorets' `{5828227C-20CF-4408-B73F-73AB70B8849F}` and
`{E99FA5E4-A1C9-4495-8BF0-D0972D58E841}` (symbol `PROTOCOLTYPE_TSTEST`, not registered by Windows).
Every other protocol is a different type, so it never reconnects to sessions made by RDP and RDP
never reconnects to its sessions; only the console (glass terminal) is compatible with both.

* **RDP-compatible (default):** the provider is registered under `{E99FA5E4-...}`. termsrv then
  treats FastRDP connections as RDP: Windows' own arbitration reconnects, takes over and prompts
  exactly as for mstsc. The other places that use `IsRDPProtocol` (termsrv 10.0.26100.9818): the
  license type stays `LICENSE_TYPE_BUILTIN` (the provider does not answer the license property),
  and the listener starts without the non-RDP-stack gate. The installer refuses if that CLSID is
  registered by something else.
* **Native (`-NativeClsid`):** FastRDP's own CLSID. The provider answers
  `SessionArbitrationEnumeration` itself with the user's active or oldest disconnected FastRDP or
  console session (the account's SID is looked up before `OnConnected`; the answer is only given if
  the logged-on token has that SID). termsrv calls it twice: the size query must return
  `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)` with the count. Sessions made by RDP are never
  named (they would fail with ERROR_CTX_SESSION_INCOMPATIBLE).

Before a session is created at all, the password is checked with a network logon (like RDP's
NLA); definite failures are reported to the client.

## Wire protocol (`crates/protocol`)

Control frames: `u32 payload_len | u16 type | u16 flags | payload`, strings as UTF-16LE with a
`u16` length. Main messages:

| Type | Direction | |
|---|---|---|
| `0x0001 HELLO`, `0x0002 AUTH`, `0x0003 BYE` | client → server → provider | session request |
| `0x0100 HELLO_ACK`, `0x0101 EVENT` | provider/server → client | progress, session id, logon, errors, input path |
| `0x0102 STREAM_INFO` | server → client | encoder, size, frame rate of the video stream |
| `0x0004 FEEDBACK` | client → server | received / lost / FEC-recovered frames, IDR request |
| `0x0005 INPUT` | client → server (→ provider or agent) | keys (scancodes), buttons, wheel, absolute mouse, lock-key sync |
| `0x0007 DISPLAY` | client → server | desired remote mode (resize, DPI change) |
| `0x0010..0x0012 CHANNEL_*` | both | virtual channel open / data fragments / close |
| `0x0103 CURSOR_SHAPE`, `0x0104 CURSOR_POS` | server → client | pointer image and position |

QUIC streams: one bidirectional control stream, input on its own stream, channel data on separate
unidirectional streams, video as datagrams.

## Video path

```
Desktop Window Manager → IddCx swap chain (FastRdpIdd)
  → staging copy → Global\FastRDP_Frame_S<session>  (3-slot seqlock, BGRA, + event)
  → fastrdp-server: BGRA → I420 → H.264 (x264 superfast/zerolatency by default)
  → transport: 28-byte header, ≤ 64 data shards per block, Reed-Solomon parity (GF(2^8))
  → QUIC datagrams → client: reassembly / FEC recovery → OpenH264 decode → wgpu shader
```

* Frames are only produced when the desktop changes; an idle desktop costs almost nothing.
* Loss handling: the client reports loss once per second; the server raises the FEC ratio and lowers
  the bitrate (bitrate scales with resolution, recovers after a clean period). An incomplete frame
  makes the client wait for an IDR frame and request it.
* `encoder.toml` selects the encoder; if it is missing or fails, x264 takes over and the client is
  told (`EncoderFallback` event).

### Shared memory (`frame-shm`, `drivers/FastRdpIdd/FastRdpShm.h`)

`Global\FastRDP_Frame_S<n>` (+ events `Global\FastRDP_FrameEvt_S<n>`, `Global\FastRDP_CtlEvt_S<n>`): header at 0, pointer block
at 1024, display-control block at 2048, pointer shape at 4096 (up to 256×256), frame slots after
that (up to 3840×2160 BGRA each). The Rust and C++ layouts are checked against each other by unit
tests.

## Display

The client asks for a size and DPI scale (`DISPLAY`) when connecting, when the window is resized
(debounced) and when it moves to a monitor with another scale. The server writes the request into
the display-control block; the driver applies it with IddCx 1.10 `IddCxMonitorUpdateModes2` and
`IddCxAdapterDisplayConfigUpdate` (scale), so any mode up to 3840×2160 works. The server writes the
first request before the driver starts, so the session comes up at the client's size.

## Input

Preferred path, the same as Microsoft's RDP stack: the provider creates the session keyboard and
mouse software devices (`TS_INPT\TS_KBD`, `TS_INPT\TS_MOU`, bound to the inbox `terminpt.sys`) and
passes their class handles to TermService in `GetInputHandles`. Input then reaches the session like
hardware input — including the lock screen, UAC prompts and Ctrl+Alt+Del. These are undocumented
interfaces; if any step fails the server falls back to `fastrdp-agent` (SendInput, which cannot
reach the secure desktop or elevated windows).

On the client, a low-level keyboard hook sends Windows key combinations to the session while the
remote window is active (like mstsc's "Apply Windows key combinations"). winit's Raw Input keyboard
registration is removed, because Windows does not call a process's own low-level hook while a window
of a process with a Raw Input keyboard registration is in front.

## Virtual channels and clipboard

Session components (rdpclip, audio, device redirection) open channels through
`IWRdsProtocolConnection::CreateVirtualChannel`. The provider hands them one end of a message-mode
pipe and relays every message to the server and on to the client (`CHANNEL_*`). The client
implements the client side of MS-RDPECLIP: text, images (DIB) and files (file contents are fetched
when copied). Channel content is never logged.

## Updates (`crates/update`)

Each release has `manifest.json` (format, version, tag, SHA-256 and size of every release file) and
`manifest.json.sig`: `ed25519 <key id> <base64 signature>` over the exact manifest bytes. The
*Sign release* workflow creates both with the private key stored as a secret of the `release`
environment (approval required) and only then publishes the draft release. `fastrdp-update.exe`
and the client accept a manifest only if it verifies with a key in `crates/update/src/keys.rs`
(current + next key, so the key can be rotated), only offer versions newer than their own, and
check size and SHA-256 of every download.

Server side: the scheduled task *FastRDP Update* (SYSTEM; at boot + every 4 hours) runs
`fastrdp-update apply --scheduled`. It defers while any non-console session is active, extracts the
package to `%ProgramFiles%\FastRDP\packages\` (Administrators only), runs its
`Install-FastRdp.ps1 -Unattended` and re-installs the previous package if that fails. The current
and the previous package are kept. `HKLM\SOFTWARE\FastRDP`: `InstalledVersion`, `AutoUpdate`.

## Files and settings

| Path | |
|---|---|
| `%ProgramFiles%\FastRDP\` | provider DLL, server, agent, updater; `packages\` (current + previous release), `scripts\` |
| `%ProgramData%\FastRDP\server\` | server TLS key and certificate (`cert.sha256`); readable by SYSTEM, Administrators, LOCAL SERVICE only |
| `%ProgramData%\FastRDP\logs\` | `provider.log`, `server.log`, `idd.log` |
| `%ProgramData%\FastRDP\encoder.toml` | encoder settings |
| `%ProgramData%\FastRDP\backup\` | registry backups and undo instructions from the provider installer |
| `%APPDATA%\FastRDP\` (client) | `known_servers` (trusted certificates), `connections` (per-server settings) |
| `%LOCALAPPDATA%\FastRDP\client.log` | client log |
