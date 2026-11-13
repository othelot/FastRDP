// FramePublisher.h - copies swap-chain frames into the per-session shared memory (FastRdpShm.h).
// Frames are converted to I420 on the render GPU first (ToI420.hlsl: 1.5 instead of 4 bytes per pixel
// to read back and copy); where that is not possible (no compute shader / typed UAV) they go as BGRA.
#pragma once
#include "Driver.h"
#include "FastRdpShm.h"

namespace FastRdp {

int64_t QpcMicros();

class FramePublisher {
public:
    struct SlotInfo { uint64_t frameNumber = 0; int64_t ptsUs = 0; uint32_t dirtyRects = 0; };

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
    // Releases the GPU objects made on `dev` (staging texture, I420 converter). Called by the swap-chain thread when it
    // stops: GPU objects must never be released from the static destructor, i.e. at DLL unload under
    // the loader lock - with a hardware GPU driver (NVIDIA) that blocked WUDFHost until UMDF killed it
    // after 60 s, and TermService waited for that on every disconnect/reconnect.
    void ReleaseGpu(ID3D11Device* dev);
    // Publishes hardware-cursor state; `shape` (nullptr = unchanged) is Height rows of `Pitch` bytes.
    void PublishCursor(bool visible, int32_t x, int32_t y, const IDDCX_CURSOR_SHAPE_INFO* info, const uint8_t* shape);

private:
    FramePublisher() = default;
    ~FramePublisher();

    std::mutex m_lock;
    // The cursor block has its own lock: under m_lock a frame does its GPU copy, conversion and Map
    // (waiting for the GPU) and the slot copy, and the cursor thread waited for all of that before it
    // could answer the OS again - at 240 Hz on a busy GPU, over and over. m_pBase / m_pHeader are set
    // once by Ensure, before any cursor thread exists.
    std::mutex m_cursorLock;
    ULONG m_SessionId = 0xFFFFFFFF;
    HANDLE m_hSection = nullptr;
    HANDLE m_hEvent = nullptr;
    HANDLE m_hCtlEvent = nullptr;
    FastRdpShmHeader* m_pHeader = nullptr;
    uint8_t* m_pBase = nullptr;
    bool m_failed = false;
    // Both paths do their GPU work and Map first, then fill the slot (seq odd until Publish commits).
    HRESULT EnsureConverter(ID3D11Device* dev, UINT width, UINT height);   // shader + textures for this device/size
    HRESULT PublishI420(ID3D11DeviceContext* ctx, ID3D11Texture2D* src, UINT width, UINT height, const SlotInfo& info, FastRdpShmSlot& slot, uint8_t* dst);
    HRESULT PublishBgra(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* src, const D3D11_TEXTURE2D_DESC& d, const SlotInfo& info, FastRdpShmSlot& slot, uint8_t* dst);
    void ReleaseConverter();

    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_Staging;      // BGRA path
    ID3D11Device* m_StagingDevice = nullptr;
    UINT m_StagingW = 0, m_StagingH = 0;
    Microsoft::WRL::ComPtr<ID3D11ComputeShader> m_Cs;       // I420 path
    Microsoft::WRL::ComPtr<ID3D11Buffer> m_CsSize;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_CsSrc;        // B8G8R8A8_UNORM copy of the surface
    Microsoft::WRL::ComPtr<ID3D11ShaderResourceView> m_CsSrv;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_CsDst;        // R8_UNORM, width x height*3/2
    Microsoft::WRL::ComPtr<ID3D11UnorderedAccessView> m_CsUav;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> m_CsStaging;
    ID3D11Device* m_CsDevice = nullptr;
    UINT m_CsW = 0, m_CsH = 0;
    ID3D11Device* m_CsUnsupported = nullptr;                // device the converter failed on (BGRA from then on)
    int64_t m_lastHeartbeat = 0;
};

} // namespace FastRdp
