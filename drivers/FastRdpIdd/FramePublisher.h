// FramePublisher.h - copies swap-chain frames into the per-session shared memory (FastRdpShm.h).
// CPU path for machines without a usable hardware encoder (WARP / software encoder). A GPU shared-texture path
// (for a hardware encoder) is planned and will sit next to this one.
#pragma once
#include "Driver.h"
#include "FastRdpShm.h"

namespace FastRdp {

int64_t QpcMicros();

class FramePublisher {
public:
    static FramePublisher& Instance();   // one per WUDFHost (UmdfHostProcessSharing=ProcessSharingDisabled)

    // Creates Global\FastRDP_Frame_S<session> + event on first use. Returns false if unavailable.
    bool Ensure(ULONG sessionId);
    HRESULT Publish(ID3D11Device* dev, ID3D11DeviceContext* ctx, IDXGIResource* surface,
                    uint64_t frameNumber, int64_t ptsUs, uint32_t dirtyRects);
    void Heartbeat();
    // Display control block (written by the server). Returns false if no new request is pending.
    bool ReadControl(FastRdpShmControl& out);
    void ReportControl(uint32_t requestId, NTSTATUS status, uint32_t width, uint32_t height, uint32_t scale);
    HANDLE ControlEvent() const { return m_hCtlEvent; }
    // Releases the GPU objects made on `dev` (staging texture). Called by the swap-chain thread when it
    // stops: GPU objects must never be released from the static destructor, i.e. at DLL unload under
    // the loader lock - with a hardware GPU driver (NVIDIA) that blocked WUDFHost until UMDF killed it
    // after 60 s, and TermService waited for that on every disconnect/reconnect.
    void ReleaseGpu(ID3D11Device* dev);
    // Publishes hardware-cursor state; `shape` (nullptr = unchanged) is Height rows of `Pitch` bytes.
    void PublishCursor(bool visible, int32_t x, int32_t y, uint32_t shapeId, const IDDCX_CURSOR_SHAPE_INFO* info, const uint8_t* shape);

private:
    FramePublisher() = default;
    ~FramePublisher();

    std::mutex m_lock;
    ULONG m_SessionId = 0xFFFFFFFF;
    HANDLE m_hSection = nullptr;
    HANDLE m_hEvent = nullptr;
    HANDLE m_hCtlEvent = nullptr;
    FastRdpShmHeader* m_pHeader = nullptr;
    uint8_t* m_pBase = nullptr;
    bool m_failed = false;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_Staging;
    ID3D11Device* m_StagingDevice = nullptr;
    UINT m_StagingW = 0, m_StagingH = 0;
    int64_t m_lastHeartbeat = 0;
};

} // namespace FastRdp
