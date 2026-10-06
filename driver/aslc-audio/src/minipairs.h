/*++

ASLC virtual audio driver — endpoint miniport pairs.

Original ASLC code. Derived from Microsoft's SysVAD sample `minipairs.h`
(Microsoft Corporation; see driver/third_party/sysvad/LICENSE-windows-driver-samples.txt).
Trimmed to a single render (playback) endpoint named ASLC: the internal
speaker topology/wave pair. Capture, HDMI, SPDIF, microphone, sideband
(Bluetooth/USB/A2DP) and multi-endpoint plumbing are removed.

--*/

#ifndef _ASLC_MINIPAIRS_H_
#define _ASLC_MINIPAIRS_H_

// ASLC exposes no capture endpoints, so the vendored adapter's capture-endpoint loop
// (`for (i = 0; i < g_cCaptureEndpoints; ...)` with a 0 count) trips C4296 ("expression
// always false"), which the WDK promotes to an error. The loop is correct and never runs.
#pragma warning(disable : 4296)

#include "speakertopo.h"
#include "speakertoptable.h"
#include "speakerwavtable.h"

NTSTATUS
CreateMiniportWaveRTSYSVAD
(
    _Out_       PUNKNOWN *,
    _In_        REFCLSID,
    _In_opt_    PUNKNOWN,
    _In_        POOL_FLAGS,
    _In_        PUNKNOWN,
    _In_opt_    PVOID,
    _In_        PENDPOINT_MINIPAIR
);

NTSTATUS
CreateMiniportTopologySYSVAD
(
    _Out_       PUNKNOWN *,
    _In_        REFCLSID,
    _In_opt_    PUNKNOWN,
    _In_        POOL_FLAGS,
    _In_        PUNKNOWN,
    _In_opt_    PVOID,
    _In_        PENDPOINT_MINIPAIR
);

//
// Describe buffer size constraints for the WaveRT render buffer.
//
static struct
{
    KSAUDIO_PACKETSIZE_CONSTRAINTS2 TransportPacketConstraints;
    KSAUDIO_PACKETSIZE_PROCESSINGMODE_CONSTRAINT AdditionalProcessingConstraints[1];
} AslcWaveRtPacketSizeConstraintsRender =
{
    {
        2 * HNSTIME_PER_MILLISECOND,                // 2 ms minimum processing interval
        FILE_BYTE_ALIGNMENT,                        // 1 byte packet size alignment
        0,                                          // no maximum packet size constraint
        2,                                          // 2 processing constraints follow
        {
            STATIC_AUDIO_SIGNALPROCESSINGMODE_DEFAULT,          // constraint for default processing mode
            128,                                                // 128 samples per processing frame
            0,                                                  // NA hns per processing frame
        },
    },
    {
        {
            STATIC_AUDIO_SIGNALPROCESSINGMODE_MOVIE,            // constraint for movie processing mode
            1024,                                               // 1024 samples per processing frame
            0,                                                  // NA hns per processing frame
        },
    }
};

const SYSVAD_DEVPROPERTY AslcWaveFilterInterfacePropertiesRender[] =
{
    {
        &DEVPKEY_KsAudio_PacketSize_Constraints2,           // Key
        DEVPROP_TYPE_BINARY,                                // Type
        sizeof(AslcWaveRtPacketSizeConstraintsRender),      // BufferSize
        &AslcWaveRtPacketSizeConstraintsRender,             // Buffer
    },
};

/*********************************************************************
* Topology/Wave bridge connection for the ASLC render endpoint.      *
*                                                                    *
*              +------+                +------+                      *
*              | Wave |                | Topo |                      *
*              |      |                |      |                      *
* System   --->|0    2|---> Loopback   |      |                      *
*              |      |                |      |                      *
* Offload  --->|1    3|--------------->|0    1|---> Line Out         *
*              |      |                |      |                      *
*              +------+                +------+                      *
*********************************************************************/
static
PHYSICALCONNECTIONTABLE AslcTopologyPhysicalConnections[] =
{
    {
        KSPIN_TOPO_WAVEOUT_SOURCE,  // TopologyIn
        KSPIN_WAVE_RENDER_SOURCE,   // WaveOut
        CONNECTIONTYPE_WAVE_OUTPUT
    }
};

static
ENDPOINT_MINIPAIR AslcSpeakerMiniports =
{
    eSpeakerDevice,
    L"TopologyASLC",                                        // must match KSNAME_TopologyASLC in the inf [Strings]
    NULL,                                                   // optional template name
    CreateMiniportTopologySYSVAD,
    &SpeakerTopoMiniportFilterDescriptor,
    0, NULL,                                                // Interface properties
    L"WaveASLC",                                            // must match KSNAME_WaveASLC in the inf [Strings]
    NULL,                                                   // optional template name
    CreateMiniportWaveRTSYSVAD,
    &SpeakerWaveMiniportFilterDescriptor,
    ARRAYSIZE(AslcWaveFilterInterfacePropertiesRender),     // Interface properties
    AslcWaveFilterInterfacePropertiesRender,
    SPEAKER_DEVICE_MAX_CHANNELS,
    SpeakerPinDeviceFormatsAndModes,
    SIZEOF_ARRAY(SpeakerPinDeviceFormatsAndModes),
    AslcTopologyPhysicalConnections,
    SIZEOF_ARRAY(AslcTopologyPhysicalConnections),
    ENDPOINT_OFFLOAD_SUPPORTED,
    SpeakerModulesWaveFilter,
    SIZEOF_ARRAY(SpeakerModulesWaveFilter),
    &SpeakerModuleNotificationDeviceId,
};

//=============================================================================
// Render miniport pairs (ASLC has exactly one render endpoint).
//
static
PENDPOINT_MINIPAIR  g_RenderEndpoints[] =
{
    &AslcSpeakerMiniports,
};

#define g_cRenderEndpoints  (SIZEOF_ARRAY(g_RenderEndpoints))

// ASLC exposes no capture endpoints.
static
PENDPOINT_MINIPAIR  g_CaptureEndpoints[] =
{
    NULL,
};

#define g_cCaptureEndpoints 0

// Total miniports = # endpoints * 2 (topology + wave).
#define g_MaxMiniports  ((g_cRenderEndpoints + g_cCaptureEndpoints) * 2)

#endif // _ASLC_MINIPAIRS_H_
