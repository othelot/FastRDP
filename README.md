# FastRDP

A remote desktop for **Windows 11 Pro** that does not use the RDP protocol.

FastRDP plugs into Windows' own Remote Desktop Services through the documented
**custom protocol provider** API (`IWRdsProtocolManager`). Windows creates a real, independent
remote session — the same kind of session mstsc gets — while the picture, input and channels travel
over FastRDP's own transport: H.264 video in QUIC datagrams with forward error correction.

No system file is patched: no termsrv.dll changes, no RDPWrap, no ServiceDll changes, and the
built-in `RDP-Tcp` listener is never modified. Every install step has an uninstall script.

> **Status: experimental.** Microsoft documents the protocol-provider API for Windows Server.
> On Windows 11 it works but is unofficial and may change with Windows updates. Parts of the input
> path use undocumented Windows internals (with a fallback). Use it on machines you control.

## Features

| | |
|---|---|
| Sessions | Independent remote sessions created by TermService; reconnects to a disconnected session; takeover between console and remote like RDP (Windows 11 Pro allows one active user) |
| Display | Remote Indirect Display Driver (IddCx 1.10); any resolution up to 3840×2160, DPI scale, resize the remote desktop with the client window |
| Video | Damage-driven H.264 (x264 by default, OpenH264 optional) over QUIC datagrams with Reed-Solomon FEC, loss feedback and adaptive bitrate |
| Input | Keyboard and mouse through the same session input devices RDP uses, so the lock screen, UAC and Ctrl+Alt+Del work (Ctrl+Alt+End sends Ctrl+Alt+Del); SendInput agent as fallback |
| Keyboard | Windows key combinations (Alt+Tab, Win, …) go to the remote session — always, only in full screen, or never |
| Pointer | Remote pointer shapes; on Windows the client shows them as the real OS cursor (no pointer lag) |
| Clipboard | Text, images and files in both directions (Windows' own rdpclip on the session side, MS-RDPECLIP) |
| Experience | Per-connection visual effects (wallpaper, animations, font smoothing, …) applied to the session only |
| Client | iced GUI with certificate confirmation like RDP, full-screen connection bar, saved per-server settings; `--headless` mode for measurements |

Not implemented yet: audio, drive redirection, multiple monitors, hardware (GPU) encoding.
The client is written to be cross-platform, but has only been built and used on Windows.

## How it works

```
 client ── QUIC/UDP 33890 (TLS 1.3, pinned certificate) ──► fastrdp-server  (service, LOCAL SERVICE)
                                                              │  local named pipe
                                                              ▼
                                                 fastrdp_provider.dll  (inside TermService)
                                                              │  OnConnected / OnReady
                                                              ▼
                                                 Windows creates the remote session
                                                              │
                         FastRdpIdd (remote display driver) ──┘ frames + pointer → shared memory → server → encoder
```

See [docs/architecture.md](docs/architecture.md) for the details.

## Repository layout

```
crates/
  protocol/    wire protocol shared by all components
  provider/    protocol provider DLL loaded by TermService (COM in-proc server)
  server/      fastrdp-server: QUIC endpoint, session relay, encoding, input and channel relay
  client/      fastrdp-client: iced GUI (+ --headless)
  agent/       fastrdp-agent: per-session helper (rdpclip, SendInput fallback)
  encoder/     H.264 encoders with x264 fallback, encoder-bench
  transport/   video datagrams: fragmentation, Reed-Solomon FEC, reassembly
  frame-shm/   shared memory between the display driver and the server
  update/      fastrdp-update: signed release manifest, verified downloads, update task
drivers/
  FastRdpIdd/  remote Indirect Display Driver (C++, UMDF 2, IddCx)
  Build-FastRdpIdd.ps1
scripts/       install / uninstall scripts (provider, display driver, server)
web/           install.ps1 (template of the one-line installer) + Cloudflare Worker for fastrdp.dev
tools/         New-FastRdpRelease.ps1 (release packages)
.github/       release signing workflow
docs/
```

## Building

Requirements (build machine, Windows x64):

* Rust (stable, edition 2024) with the MSVC toolchain
* libx264, static: `vcpkg install x264:x64-windows-static` with [vcpkg](https://github.com/microsoft/vcpkg)
  cloned to `%USERPROFILE%\vcpkg` (elsewhere: set `VCPKG_ROOT`, or `FASTRDP_X264_DIR` to a directory
  containing `include\x264.h` and `lib\libx264.lib`)
* For the display driver: Visual Studio 2026 with the Windows Driver Kit 10.0.28000

```powershell
cargo build --workspace --release        # target\release\: fastrdp-server.exe, fastrdp-agent.exe,
                                         #   fastrdp_provider.dll, fastrdp-client.exe
cargo test --workspace
pwsh drivers\Build-FastRdpIdd.ps1        # target\driver\FastRdpIdd\ + FastRdpDriverPublisher.cer
```

`pwsh tools\New-FastRdpRelease.ps1` builds everything and writes the release files (server package,
client package, `install.ps1`, `SHA256SUMS.txt`) to `target\release-pkg\`. With `-Publish` it uploads
them as a draft GitHub release and starts the *Sign release* workflow; after approval in the
`release` environment, the workflow adds the signed `manifest.json` and publishes the release.
Updates are only accepted with that signature.

`Build-FastRdpIdd.ps1` signs the driver with a self-signed certificate it creates in the build
user's certificate store (private key not exportable). This is not a Microsoft signature: the
installer makes the target machine trust that certificate.

## Installing (the computer you connect to)

On Windows 11 Pro, in PowerShell (an elevated window is opened automatically):

```powershell
irm https://fastrdp.dev | iex                                          # install or update
& ([scriptblock]::Create((irm https://fastrdp.dev))) -Uninstall       # remove (-Purge: also keys, logs)
& ([scriptblock]::Create((irm https://fastrdp.dev))) -AutoUpdate Off  # install without automatic updates
```

The script downloads the release package from GitHub, checks its SHA-256, asks before changing
anything and runs the install scripts below. Log: `%ProgramData%\FastRDP\logs\setup.log`.
The client is `fastrdp-client-<version>-x64.zip` on the [releases page](https://github.com/othelot/fastrdp/releases).

### Updates

The installer registers the scheduled task **FastRDP Update** (SYSTEM). Every few hours
`fastrdp-update.exe` reads the latest release's `manifest.json`, checks its Ed25519 signature
against the public keys built into it, and — if the release is newer and nobody is connected
remotely (installing restarts the Remote Desktop service) — downloads and verifies the package and
runs its `Install-FastRdp.ps1`. If that fails, the previous version is installed again. Log:
`%ProgramData%\FastRDP\logs\update.log`. `fastrdp-update check` shows the state, `fastrdp-update apply`
updates now. Turn automatic updates off with `-AutoUpdate Off`.

The client looks for a newer release when it starts and offers *Update and restart*
(`--no-update-check` turns this off). Client and server refuse to talk if their protocol versions differ.

### Manual install from a build

Elevated PowerShell, from the repository root after building:

```powershell
# 1. remote display driver (trusts the self-signed driver certificate)
scripts\Install-FastRdpIdd.ps1 -PackageDir target\driver\FastRdpIdd -CertFile target\driver\FastRdpDriverPublisher.cer

# 2. protocol provider: adds a "FastRDP" listener next to RDP-Tcp (restarts TermService,
#    which disconnects existing RDP sessions)
scripts\Install-FastRdpProvider.ps1 -DllPath target\release\fastrdp_provider.dll -RestartTermService

# 3. server service, session agent task and firewall rule (UDP 33890, Private profile by default)
scripts\Install-FastRdpServer.ps1 -BinDir target\release
```

(`Install-FastRdp.ps1 -PackageDir <extracted release package>` runs all three, as the online
installer does.)

The server prints its certificate fingerprint (also in `%ProgramData%\FastRDP\server\cert.sha256`).
Compare it with what the client shows on the first connection.

Uninstall in reverse order: `Uninstall-FastRdpServer.ps1`, `Uninstall-FastRdpProvider.ps1 -RestartTermService`,
`Uninstall-FastRdpIdd.ps1`. The provider installer keeps a registry backup and emergency undo
instructions in `%ProgramData%\FastRDP\backup\install-*\UNDO.txt`.

The account you sign in with needs the right to log on through Remote Desktop Services (by default
Administrators and members of *Remote Desktop Users*).

## Connecting

Run `fastrdp-client.exe`, enter `host` or `host:port` and the user name (`name`, `DOMAIN\name` or
`name@domain`), and confirm the server certificate on the first connection.

* **Ctrl+Alt+End** sends Ctrl+Alt+Del, **Ctrl+Alt+Break** toggles full screen; maximizing the window
  also switches to full screen.
* Settings are saved per server in `%APPDATA%\FastRDP`; the client log is
  `%LOCALAPPDATA%\FastRDP\client.log`.
* `fastrdp-client --help` lists the command-line options (`--headless`, `--pin`, `--experience`, …).

## Configuration

| Where | What |
|---|---|
| `%ProgramData%\FastRDP\encoder.toml` | encoder (`x264` / `openh264`), bitrate, x264 preset — see `crates/encoder/src/settings.rs` |
| `HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\FastRDP` | `FastRdpVirtualChannels` (1), `FastRdpDeviceInput` (1), `FastRdpIddHardwareId` |
| `fastrdp-server` service arguments | `--bind 0.0.0.0:33890`, `--fec`, `--listener` |
| Logs | `%ProgramData%\FastRDP\logs\` (provider, server, driver) |

## Security notes

* The password is sent once, inside QUIC/TLS 1.3, to a server whose certificate the user confirmed
  (fingerprint pinning, like RDP's certificate prompt). The server hands it to Windows over a local
  pipe that only SYSTEM, LOCAL SERVICE, Administrators and TermService can open; it is wiped from
  memory after use and never logged.
* There is no Network Level Authentication and no rate limiting of failed logons yet. Do not expose
  UDP 33890 to the internet; use it on a LAN or behind a VPN.
* Installing the display driver adds a self-signed certificate to the machine's trusted roots and
  trusted publishers. `Uninstall-FastRdpIdd.ps1` removes it.

## License

FastRDP's source code is released under the [MIT License](LICENSE).

The encoder statically links **x264**, which is licensed under the GNU GPL v2 or later. Binaries
built with x264 (every `fastrdp-server.exe`) are therefore covered by the GPL when distributed.
OpenH264 is BSD-2-Clause. Other dependencies are listed in `Cargo.lock` with their own licenses.
