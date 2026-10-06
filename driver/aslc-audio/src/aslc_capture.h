/*++

ASLC virtual audio driver — PCM capture / user-mode handoff.

Original ASLC code. The virtual render endpoint's miniport already computes the
newly-rendered run of PCM (the same bytes it hands to the sample WAV dumper); this
module captures that run into an in-kernel ring buffer and exposes it to user mode
through a small control device:

    \\.\AslcAudio   (symbolic link -> \Device\AslcAudio)

    IOCTL_ASLC_GET_FORMAT : returns the current stream format
    IOCTL_ASLC_GET_PCM    : returns up to N bytes of captured PCM

This is the kernel/user boundary. No ASLC protocol/transport logic lives here; the
companion owns everything above the raw PCM. The mechanism is intentionally simple
(shared ring + two IOCTLs); it can later be upgraded to a shared-section zero-copy
ring without changing the boundary semantics.

--*/

#pragma once

#include <ntddk.h>

#define ASLC_CAPTURE_DEVICE_NAME   L"\\Device\\AslcAudio"
#define ASLC_CAPTURE_SYMLINK_NAME  L"\\DosDevices\\AslcAudio"

//
// IOCTLs. Both are METHOD_BUFFERED and read-only.
//   GET_PCM:   input  = ULONG requested byte count
//              output = PCM bytes; IoStatus.Information = bytes returned
//   GET_FORMAT: output = ASLC_CAPTURE_FORMAT
//
#define ASLC_IOCTL_GET_FORMAT CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900, METHOD_BUFFERED, FILE_READ_DATA)
#define ASLC_IOCTL_GET_PCM    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x901, METHOD_BUFFERED, FILE_READ_DATA)

// Matches WAVEFORMATEX for PCM; kept as a stable, user-mode-friendly struct.
#pragma warning(push)
#pragma warning(disable : 4200)
typedef struct _ASLC_CAPTURE_FORMAT
{
    ULONG   SampleRate;
    ULONG   AvgBytesPerSec;
    USHORT  Channels;
    USHORT  BitsPerSample;
    USHORT  BlockAlign;
    USHORT  Valid;
} ASLC_CAPTURE_FORMAT, *PASLC_CAPTURE_FORMAT;
#pragma warning(pop)

// Create the control device (once, at DriverEntry). Idempotent.
NTSTATUS AslcCaptureCreateDevice(_In_ PDRIVER_OBJECT DriverObject);

// Record the negotiated stream format (called on SetFormat).
VOID AslcCaptureSetFormat(_In_opt_ const PVOID WaveFormat, _In_ ULONG Length);

// Append a rendered PCM run to the ring (called from the render stream on GetPositions).
VOID AslcCaptureWrite(_In_reads_bytes_(Length) const VOID* Data, _In_ ULONG Length);
