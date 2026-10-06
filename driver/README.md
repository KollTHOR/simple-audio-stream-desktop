# ASLC Windows virtual audio sink (driver)

The ASLC companion presents a **real Windows playback device named `ASLC`**. Selecting it as
the system output routes PCM into the ASLC node, which later packetizes and transports it
(ASLC protocol) to a receiver. This directory is the **kernel driver** for that endpoint.

```
Windows Audio Engine
        │  (user selects "ASLC" as output)
        ▼
[ kernel ] ASLC virtual render endpoint (PortCls / WaveRT, WDM)
        │  render buffer → harvested PCM
        ▼
[ kernel ] PCM export (ring buffer; user-mode handoff)
        │
        ▼
[ user ]   ASLC companion (Rust) — owns ASLC framing/buffering/transport/discovery
```

The driver is deliberately minimal: **one render (playback) endpoint only**. No capture/mic,
no APOs/effects, no offload, no Bluetooth/USB audio, no Android specifics. All ASLC logic
(packetization, transport, discovery) lives in the Rust companion, never in the kernel.

## Foundation & licensing

Built as our own implementation informed by Microsoft's **SysVAD** sample
(<https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad>), with
**Scream**, **AudioMirror** and **Virtual-Audio-Driver** studied only as technical references
(not runtime dependencies, not copied).

- `third_party/sysvad/` holds the pristine, unmodified SysVAD files we started from, with the
  upstream `LICENSE-windows-driver-samples.txt`. SysVAD sample code is Microsoft-licensed
  (MIT repo license / MS-PL notices on the sample); all notices are preserved.
- `aslc-audio/` holds original ASLC code and the SysVAD-derived files we adapt (each adapted
  file carries a provenance header). Nothing Android/USB/ASLC-protocol lives here.

## Layout

```
driver/
  README.md                 this file
  third_party/sysvad/       pristine Microsoft SysVAD subset + upstream license
  aslc-audio/
    src/                    our driver sources (adapted SysVAD + ASLC harvest/export)
    inf/                    aslc-audio.inx  (stamped to .inf at build)
    aslc-audio.vcxproj      WDK driver project
  build/build-driver.ps1    build the driver (x64)
  tools/enable-wdk-toolset.ps1   one-time: wire the WDK MSBuild toolset into VS Build Tools
  install/install-dev.ps1   test-signing + install the driver for development
```

## Build toolchain (verified on this machine)

| Item | Value |
|---|---|
| Toolset | `WindowsKernelModeDriver10.0` (from the WDK VSIX, deployed into VS Build Tools) |
| Windows SDK target | `10.0.26100.0` (the installed full SDK; 22621 is WDK km-only) |
| Spectre libs | disabled for dev (`Driver_SpectreMitigation=false`) |
| Post-build signing | disabled (`SignMode=Off`) — we sign explicitly |
| Confirmed | a minimal driver produced `test.sys` via MSBuild with this setup |

VS Build Tools alone does **not** ship the WDK toolset; the WDK's `WDK.vsix` refuses to attach
to Build Tools (VSIX error 2003). `tools/enable-wdk-toolset.ps1` deploys the VSIX's bundled
MSBuild overlay (`$MSBuild\Microsoft\VC\v170\...`) into the Build Tools MSBuild tree — exactly
what the IDE installer would do. Run it once (elevated).

## Status / roadmap

- [x] Driver build toolchain proven (WDK toolset wired; `test.sys` builds)
- [x] SysVAD render subset vendored with license/provenance
- [ ] Port to a minimal render-only ASLC endpoint (drop offload/APO/keyword/BT/USB/mic)
- [ ] Rename endpoint/INF display strings to `ASLC`
- [ ] PCM harvest from the WaveRT render buffer
- [ ] PCM export to user mode (POC: IOCTL pull / WAV; then shared ring + event)
- [ ] `aslc-audio.inx`, version resources, catalog
- [ ] Dev install script (test-signing) → verify `ASLC` in Sound settings
- [ ] Tiny user-mode test program → WAV → verify
 (Android/USB integration is explicitly out of scope here.)
