/*++

ASLC virtual audio driver — PCM capture / user-mode handoff (implementation).
See aslc_capture.h for the design and the IOCTL contract.

--*/

#include <sysvad.h>
#include <wdmsec.h>
#include "aslc_capture.h"

// 4 MB ring is ample for the POC and independent of stream format.
#define ASLC_CAPTURE_RING_BYTES (4u * 1024u * 1024u)

typedef struct _ASLC_CAPTURE
{
    KSPIN_LOCK          Lock;
    PUCHAR              Buffer;      // ring storage
    ULONG               Size;        // ring size in bytes
    ULONGLONG           WritePos;    // monotonically increasing byte counters
    ULONGLONG           ReadPos;
    ASLC_CAPTURE_FORMAT Format;
    BOOLEAN             Initialized;
} ASLC_CAPTURE;

static ASLC_CAPTURE g_Capture = {0};
static WDFDEVICE    g_ControlDevice = NULL;

// Forward declaration.
VOID AslcEvtIoDeviceControl(
    _In_ WDFQUEUE   Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t     OutputBufferLength,
    _In_ size_t     InputBufferLength,
    _In_ ULONG      IoControlCode);

static VOID AslcCaptureRead(_Out_writes_bytes_(MaxLen) PVOID Dst, _In_ ULONG MaxLen, _Out_ PULONG Written)
{
    ULONG written = 0;
    KIRQL irql;
    KeAcquireSpinLock(&g_Capture.Lock, &irql);
    if (g_Capture.Initialized)
    {
        ULONGLONG avail = g_Capture.WritePos - g_Capture.ReadPos;
        ULONG n = (avail > MaxLen) ? MaxLen : (ULONG)avail;
        ULONG done = 0;
        PUCHAR dst = (PUCHAR)Dst;
        while (done < n)
        {
            ULONG off = (ULONG)((g_Capture.ReadPos + done) & (g_Capture.Size - 1));
            ULONG chunk = g_Capture.Size - off;
            if (chunk > n - done) chunk = n - done;
            RtlCopyMemory(dst + done, g_Capture.Buffer + off, chunk);
            done += chunk;
        }
        g_Capture.ReadPos += n;
        written = n;
    }
    KeReleaseSpinLock(&g_Capture.Lock, irql);
    *Written = written;
}

static VOID AslcCaptureGetFormat(_Out_ PASLC_CAPTURE_FORMAT Out)
{
    KIRQL irql;
    KeAcquireSpinLock(&g_Capture.Lock, &irql);
    *Out = g_Capture.Format;
    KeReleaseSpinLock(&g_Capture.Lock, irql);
}

NTSTATUS AslcCaptureCreateDevice(_In_ PDRIVER_OBJECT DriverObject)
{
    UNREFERENCED_PARAMETER(DriverObject);
    NTSTATUS status;
    WDFDRIVER driver = WdfGetDriver();
    if (driver == NULL) return STATUS_UNSUCCESSFUL;
    if (g_ControlDevice != NULL) return STATUS_SUCCESS; // idempotent

    KeInitializeSpinLock(&g_Capture.Lock);

    g_Capture.Size = ASLC_CAPTURE_RING_BYTES;
    g_Capture.Buffer = (PUCHAR)ExAllocatePool2(POOL_FLAG_NON_PAGED, g_Capture.Size, 'csLA');
    if (g_Capture.Buffer == NULL) return STATUS_INSUFFICIENT_RESOURCES;
    g_Capture.WritePos = 0;
    g_Capture.ReadPos = 0;
    g_Capture.Initialized = TRUE;

    // Control device, world-accessible (same SDDL family as the audio endpoint).
    PWDFDEVICE_INIT init = WdfControlDeviceInitAllocate(driver, &SDDL_DEVOBJ_SYS_ALL_ADM_RWX_WORLD_RWX_RES_RWX);
    if (init == NULL)
    {
        ExFreePoolWithTag(g_Capture.Buffer, 'csLA');
        g_Capture.Buffer = NULL;
        g_Capture.Initialized = FALSE;
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    WdfDeviceInitSetDeviceType(init, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetCharacteristics(init, FILE_DEVICE_SECURE_OPEN, FALSE);
    WdfDeviceInitSetExclusive(init, FALSE);

    UNICODE_STRING devName;
    RtlInitUnicodeString(&devName, ASLC_CAPTURE_DEVICE_NAME);
    status = WdfDeviceInitAssignName(init, &devName);
    if (!NT_SUCCESS(status)) { WdfDeviceInitFree(init); return status; }

    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    status = WdfDeviceCreate(&init, &attrs, &g_ControlDevice);
    if (!NT_SUCCESS(status)) { WdfDeviceInitFree(init); return status; }

    UNICODE_STRING symlink;
    RtlInitUnicodeString(&symlink, ASLC_CAPTURE_SYMLINK_NAME);
    status = WdfDeviceCreateSymbolicLink(g_ControlDevice, &symlink);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG qcfg;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&qcfg, WdfIoQueueDispatchSequential);
    qcfg.EvtIoDeviceControl = AslcEvtIoDeviceControl;
    status = WdfIoQueueCreate(g_ControlDevice, &qcfg, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    if (!NT_SUCCESS(status)) return status;

    WdfControlFinishInitializing(g_ControlDevice);
    DbgPrint("ASLC: capture control device ready (%lu byte ring)\n", g_Capture.Size);
    return STATUS_SUCCESS;
}

VOID AslcEvtIoDeviceControl(
    _In_ WDFQUEUE   Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t     OutputBufferLength,
    _In_ size_t     InputBufferLength,
    _In_ ULONG      IoControlCode)
{
    UNREFERENCED_PARAMETER(Queue);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    size_t   info = 0;

    switch (IoControlCode)
    {
    case ASLC_IOCTL_GET_FORMAT:
    {
        PASLC_CAPTURE_FORMAT out = NULL;
        size_t outLen = 0;
        if (NT_SUCCESS(WdfRequestRetrieveOutputBuffer(Request, sizeof(ASLC_CAPTURE_FORMAT), (PVOID*)&out, &outLen)))
        {
            AslcCaptureGetFormat(out);
            info = sizeof(ASLC_CAPTURE_FORMAT);
            status = STATUS_SUCCESS;
        }
        else
        {
            status = STATUS_BUFFER_TOO_SMALL;
        }
        break;
    }
    case ASLC_IOCTL_GET_PCM:
    {
        ULONG req = 0;
        PVOID in = NULL;
        size_t inLen = 0;
        if (InputBufferLength >= sizeof(ULONG) &&
            NT_SUCCESS(WdfRequestRetrieveInputBuffer(Request, sizeof(ULONG), &in, &inLen)))
        {
            req = *(ULONG*)in;
        }
        PVOID out = NULL;
        size_t outLen = 0;
        if (NT_SUCCESS(WdfRequestRetrieveOutputBuffer(Request, 1, &out, &outLen)))
        {
            ULONG n = (req < (ULONG)outLen) ? req : (ULONG)outLen;
            ULONG written = 0;
            AslcCaptureRead(out, n, &written);
            info = written;
            status = STATUS_SUCCESS;
        }
        else
        {
            status = STATUS_BUFFER_TOO_SMALL;
        }
        break;
    }
    default:
        break;
    }

    WdfRequestCompleteWithInformation(Request, status, info);
}

VOID AslcCaptureSetFormat(_In_opt_ const PVOID WaveFormat, _In_ ULONG Length)
{
    if (WaveFormat == NULL || Length < sizeof(WAVEFORMATEX)) return;
    const WAVEFORMATEX* wf = (const WAVEFORMATEX*)WaveFormat;
    KIRQL irql;
    KeAcquireSpinLock(&g_Capture.Lock, &irql);
    g_Capture.Format.SampleRate    = wf->nSamplesPerSec;
    g_Capture.Format.Channels      = wf->nChannels;
    g_Capture.Format.BitsPerSample = wf->wBitsPerSample;
    g_Capture.Format.BlockAlign    = wf->nBlockAlign;
    g_Capture.Format.AvgBytesPerSec = wf->nAvgBytesPerSec;
    g_Capture.Format.Valid         = 1;
    KeReleaseSpinLock(&g_Capture.Lock, irql);
    DbgPrint("ASLC: stream format %lu Hz, %u ch, %u-bit\n",
             wf->nSamplesPerSec, wf->nChannels, wf->wBitsPerSample);
}

VOID AslcCaptureWrite(_In_reads_bytes_(Length) const VOID* Data, _In_ ULONG Length)
{
    if (Data == NULL || Length == 0) return;
    KIRQL irql;
    KeAcquireSpinLock(&g_Capture.Lock, &irql);
    if (g_Capture.Initialized)
    {
        const UCHAR* src = (const UCHAR*)Data;
        ULONG done = 0;
        while (done < Length)
        {
            ULONG off = (ULONG)((g_Capture.WritePos + done) & (g_Capture.Size - 1));
            ULONG chunk = g_Capture.Size - off;
            if (chunk > Length - done) chunk = Length - done;
            RtlCopyMemory(g_Capture.Buffer + off, src + done, chunk);
            done += chunk;
        }
        g_Capture.WritePos += Length;
        // Drop oldest if the reader falls behind.
        if (g_Capture.WritePos - g_Capture.ReadPos > g_Capture.Size)
        {
            g_Capture.ReadPos = g_Capture.WritePos - g_Capture.Size;
        }
    }
    KeReleaseSpinLock(&g_Capture.Lock, irql);
}
