// ASLC virtual audio driver — audio-engine-node build shim.
//
// The vendored SysVAD engine-node sources (MiniportAudioEngineNode.cpp,
// MiniportStreamAudioEngineNode.cpp) assume SYSVAD_BTH_BYPASS / SYSVAD_USB_SIDEBAND are defined
// and reference the sideband device (IsSidebandDevice() / m_pSidebandDevice). ASLC has no
// sideband devices, so those guard-gated members are absent from the miniport class.
//
// This translation unit provides inert shims and then includes the vendored sources unchanged,
// so their sideband branches compile; they are never executed (IsSidebandDevice() is FALSE).
//
// Transitional: the offload / audio-engine-node plumbing is removed when the driver is reduced
// to the minimal ASLC render endpoint, at which point this file goes away.

#include <sysvad.h>

struct AslcSidebandShim
{
    BOOL     IsVolumeSupported(_In_ eDeviceType) const { return FALSE; }
    PVOID    GetVolumeSettings(_In_ eDeviceType, _Out_ PULONG) const { return nullptr; }
    NTSTATUS GetVolume(_In_ eDeviceType, _In_ LONG, _Out_ PLONG) const { return STATUS_SUCCESS; }
    NTSTATUS SetVolume(_In_ eDeviceType, _In_ LONG, _In_ LONG) const { return STATUS_SUCCESS; }
    BOOL     IsMuteSupported(_In_ eDeviceType) const { return FALSE; }
    PVOID    GetMuteSettings(_In_ eDeviceType, _Out_ PULONG) const { return nullptr; }
    LONG     GetMute(_In_ eDeviceType, _In_ LONG) const { return 0; }
    NTSTATUS SetMute(_In_ eDeviceType, _In_ LONG, _In_ LONG) const { return STATUS_SUCCESS; }
};

static AslcSidebandShim g_aslcSidebandShim;

#define m_pSidebandDevice  (&g_aslcSidebandShim)
#define IsSidebandDevice() (FALSE)

#include "MiniportAudioEngineNode.cpp"
#include "MiniportStreamAudioEngineNode.cpp"
