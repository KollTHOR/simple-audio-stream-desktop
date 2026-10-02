# ASLC Desktop Node (Windows/Linux)

Companion to [Simple Audio Stream](https://github.com/KollTHOR/simple-audio-stream). Runs on a
desktop and acts as the **USB host**, streaming transparent PCM to the Android receiver over AOA
(Android Open Accessory). One cross-platform Rust core targets both Windows and Linux nodes.

Target topology:
```
Windows/Linux (this node)  --USB / AOA (host->accessory)-->  Android  -->  output / USB DAC
```

## Status: Milestone 0 (hardware-free protocol core)

Implemented and tested (on Linux, no hardware):
- `aslc` framing + control-plane codecs — **byte-for-byte** the Android wire format, locked by
  golden-vector conformance tests.
- TCP transport (dev/test byte pipe) implementing the `Transport` trait.
- Negotiation state machine (the host side) + a localhost end-to-end self-test.
- **M1** AOA host transport (`src/aoa.rs`) over `nusb`: device enumeration, the AOA control handshake
  (`GET_PROTOCOL`/`SEND_STRING×5`/`START`), accessory-mode re-enumeration, bulk-endpoint discovery, and
  `io::Read`/`io::Write` adapters over the bulk pipe — behind the *same* `Transport` trait, so the M0
  protocol code is unchanged. `list-usb` + `aoa` subcommands are wired.

Verified on real hardware (HiBy M300, Windows, 2026-10-02):
- AOA v2 works on the M300: `GET_PROTOCOL` answered v2, `START` flipped the gadget to accessory
  mode (`18D1:2D01`, persists across replug) from the desktop node.
- Bulk pipe opened: after binding WinUSB to the accessory interface (Zadig; see M1 notes below)
  the node claims interface 0 and the pipe is live.
- Stands at the last boundary: the **Android app must be running and hold the accessory**
  (`openAccessory` + accessory permission). The nightly on the device (b169) has no path to obtain
  that grant (its manifest filter is on the service, which broadcasts cannot start; there is no
  permission-request flow) — a device-app-side decision, tracked in the Android repo, not done here.

Remaining roadmap:
- **M2** WASAPI loopback capture (`cpal`) → convert/resample → paced `PCM_DATA` streaming.
- **M3** hot-plug recovery, backpressure, stats, CLI polish.
- **M4** GUI.

## Build & test

```sh
cargo build
cargo test            # unit + golden-vector conformance
cargo run -- selftest # end-to-end negotiation+stream over a local loopback (no hardware)
cargo run -- list-usb # enumerate USB devices (find your phone's VID:PID)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Requires a stable Rust toolchain (1.75+). M1 pulls `nusb` (WinUSB on Windows / usbfs on Linux) +
`futures-lite`; the M0 protocol core stays dependency-free.

## M1 bring-up on Windows

1. Toolchain: `winget install Rustlang.Rustup` + VS Build Tools 2022 with the *"Desktop development
   with C++"* workload (Rust links against MSVC).
2. `git clone https://github.com/KollTHOR/simple-audio-stream-desktop.git && cargo build`.
3. Cable the phone, run `aslc_node list-usb` — it shows what nusb can see (on Windows that requires
   a WinUSB-bound interface; see requirements below).
4. Run `aslc_node aoa --vid 18d1 --pid <pid>` — or `--accessory` when the phone already enumerates
   in accessory mode (it stays there across replug once the handshake ran).

### M1 driver requirements (Windows), learned the hard way
- **The AOA handshake needs *any* WinUSB-bound interface on the phone to open a handle**
  (device-directed vendor control transfers go through a claimed interface on Windows; the
  device-level blocking control nusb offers isn't supported on Windows). In File-transfer/MTP mode
  the only interface is WPD-owned and the claim fails — that wall is real and was hit.
  For bring-up, USB debugging was enabled once: Windows auto-binds WinUSB to the ADB interface, and
  the node performed the handshake through it. **A production installer should bind WinUSB to the
  phone's pre-handshake interface itself (signed device INF) so end users never touch Developer
  options** — see `platform/winusb/aslc_aoa.inf` for the accessory half and the same pattern for it.
- **The M300 publishes no MS OS descriptors for the accessory data interface (MI_00)** — Windows
  leaves it unbound ("Error" in Device Manager) and nusb cannot claim it. Bind it once with
  **Zadig → WinUSB** (per machine). `platform/winusb/aslc_aoa.inf` carries the matching rules and
  documents the production path (pnputil rejects unsigned third-party INFs; a signed Inf2Cat
  catalog is required). Devices that do ship AOA MS OS descriptors need no INF at all.

### The last boundary before first audio
Everything up to the pipe is verified; the pipe then waits on the **device-side app**: it must be
running and hold the accessory (`openAccessory`) before it answers HELLO/CAPABILITIES. Nightly
b169 has no way to get there without developer tooling (its USB service can't be started by the
ACCESSORY_ATTACHED broadcast on modern Android, and no accessory-permission request flow exists).
That is an Android-repo decision, deliberately *not* worked around from this side.

### Useful CLI for bring-up
`aslc_node aoa --vid 18d1 --pid <pid> [--accessory] [--tone] [--for <s>] [--wait <s>]`
— `--tone` streams a 440 Hz sine in the negotiated geometry, `--for` sets duration,
`--wait` bounds the inbound wait so a silent device prints diagnostics instead of hanging.

## Shared protocol

The wire contract lives in [`docs/ASLC_PROTOCOL.md`](docs/ASLC_PROTOCOL.md) and is generated to match
the Android implementation. If the Android codecs change, regenerate the golden file:

```sh
# from the Android repo root
./gradlew :app:testDebugUnitTest --tests "*AslcGoldenVectorGeneratorTest*"
cp app/build/golden/aslc_golden.hex <this-repo>/testdata/aslc_golden.hex
```
Then `cargo test` here will flag any divergence per-vector.

## Layout

```
src/
  frame.rs         ASLC header + BE primitives + FrameReader/Writer (transport-agnostic)
  payload.rs       control-plane payload codecs
  format.rs        PcmFormat (LE interleaved PCM geometry)
  capabilities.rs  PcmCapabilities + pure narrow()
  receiver.rs      host-side inbound parser + negotiation state machine
  transport.rs     Transport trait + TCP impl (AOA lands at M1)
  bin/aslc_node.rs CLI + `selftest`
tests/golden_vectors.rs   cross-language byte conformance vs Android
testdata/aslc_golden.hex  golden vectors generated by the Android codecs
docs/ASLC_PROTOCOL.md     the shared wire spec
```
