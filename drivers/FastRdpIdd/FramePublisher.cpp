#include "FramePublisher.h"
#include <sddl.h>
#include "ToI420.h"   // g_ToI420: cs_5_0 bytecode of ToI420.hlsl (FxCompile, $(IntDir))

using namespace Microsoft::WRL;

namespace FastRdp {

int64_t QpcMicros()
{
    static LARGE_INTEGER freq = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return f; }();
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    return static_cast<int64_t>((c.QuadPart / freq.QuadPart) * 1000000 + (c.QuadPart % freq.QuadPart) * 1000000 / freq.QuadPart);
}

FramePublisher& FramePublisher::Instance()
{
    static FramePublisher s;
    return s;
}

FramePublisher::~FramePublisher()
{
    // Static destructor = DLL unload under the loader lock while the process exits: never call into
    // the GPU driver here (see ReleaseGpu). Normally already released; otherwise leaked on purpose.
    m_Staging.Detach();
    m_StagingDevice = nullptr;
    m_Cs.Detach();
    m_CsSize.Detach();
    m_CsSrc.Detach();
    m_CsSrv.Detach();
    m_CsDst.Detach();
    m_CsUav.Detach();
    m_CsStaging.Detach();
    m_CsDevice = nullptr;
    if (m_pBase) UnmapViewOfFile(m_pBase);
    if (m_hSection) CloseHandle(m_hSection);
    if (m_hEvent) CloseHandle(m_hEvent);
    if (m_hCtlEvent) CloseHandle(m_hCtlEvent);
}

void FramePublisher::ReleaseGpu(ID3D11Device* dev)
{
    std::lock_guard<std::mutex> g(m_lock);
    if (m_Staging && (dev == nullptr || m_StagingDevice == dev)) {
        m_Staging.Reset();
        m_StagingDevice = nullptr;
        m_StagingW = m_StagingH = 0;
    }
    if (m_CsDevice && (dev == nullptr || m_CsDevice == dev)) ReleaseConverter();
    if (dev == nullptr || m_CsUnsupported == dev) m_CsUnsupported = nullptr;
}

void FramePublisher::ReleaseConverter()
{
    m_CsStaging.Reset();
    m_CsUav.Reset();
    m_CsDst.Reset();
    m_CsSrv.Reset();
    m_CsSrc.Reset();
    m_CsSize.Reset();
    m_Cs.Reset();
    m_CsDevice = nullptr;
    m_CsW = m_CsH = 0;
}

bool FramePublisher::ReadControl(FastRdpShmControl& out)
{
    if (!m_pBase) return false;
    auto* c = reinterpret_cast<FastRdpShmControl*>(m_pBase + FASTRDP_SHM_CONTROL_OFFSET);
    for (int i = 0; i < 8; i++) {
        int64_t s1 = InterlockedCompareExchange64(&c->seq, 0, 0);
        if (s1 & 1) { YieldProcessor(); continue; }
        MemoryBarrier();
        FastRdpShmControl copy;
        memcpy(&copy, c, sizeof(copy));
        MemoryBarrier();
        if (InterlockedCompareExchange64(&c->seq, 0, 0) != s1) continue;
        out = copy;
        return copy.request_id != 0 && copy.request_id != c->applied_id;
    }
    return false;
}

void FramePublisher::ReportControl(uint32_t requestId, NTSTATUS status, uint32_t width, uint32_t height, uint32_t scale)
{
    if (!m_pBase) return;
    auto* c = reinterpret_cast<FastRdpShmControl*>(m_pBase + FASTRDP_SHM_CONTROL_OFFSET);
    c->applied_status = status;
    c->applied_width = width;
    c->applied_height = height;
    c->applied_scale = scale;
    MemoryBarrier();
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&c->applied_id), static_cast<LONG>(requestId));
}

bool FramePublisher::Ensure(ULONG sessionId)
{
    std::lock_guard<std::mutex> g(m_lock);
    if (m_pBase) return true;
    if (m_failed) return false;
    m_failed = true; // only one attempt per host process; logged below

    // SYSTEM, LOCAL SERVICE (fastrdp-server, this host), Administrators (diagnostics). Nothing else.
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(L"D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)", SDDL_REVISION_1, &sd, nullptr)) {
        Log(L"FramePublisher: SDDL conversion failed %lu", GetLastError());
        return false;
    }
    SECURITY_ATTRIBUTES sa{ sizeof(sa), sd, FALSE };
    wchar_t name[64], evName[64], ctlName[64];
    _snwprintf_s(name, _TRUNCATE, L"Global\\FastRDP_Frame_S%lu", sessionId);
    _snwprintf_s(evName, _TRUNCATE, L"Global\\FastRDP_FrameEvt_S%lu", sessionId);
    _snwprintf_s(ctlName, _TRUNCATE, L"Global\\FastRDP_CtlEvt_S%lu", sessionId);
    const uint64_t total = FASTRDP_SHM_TOTAL_BYTES;
    m_hSection = CreateFileMappingW(INVALID_HANDLE_VALUE, &sa, PAGE_READWRITE, static_cast<DWORD>(total >> 32),
                                    static_cast<DWORD>(total & 0xFFFFFFFF), name);
    DWORD err = GetLastError();
    m_hEvent = CreateEventW(&sa, FALSE, FALSE, evName);
    DWORD errEv = GetLastError();
    m_hCtlEvent = CreateEventW(&sa, FALSE, FALSE, ctlName);
    LocalFree(sd);
    if (!m_hSection || !m_hEvent) {
        Log(L"FramePublisher: create %s -> %lu, %s -> %lu", name, m_hSection ? 0 : err, evName, m_hEvent ? 0 : errEv);
        return false;
    }
    m_pBase = static_cast<uint8_t*>(MapViewOfFile(m_hSection, FILE_MAP_ALL_ACCESS, 0, 0, 0));
    if (!m_pBase) {
        Log(L"FramePublisher: MapViewOfFile failed %lu", GetLastError());
        return false;
    }
    m_pHeader = reinterpret_cast<FastRdpShmHeader*>(m_pBase);
    m_pHeader->version = FASTRDP_SHM_VERSION;
    m_pHeader->header_size = sizeof(FastRdpShmHeader);
    m_pHeader->slot_count = FASTRDP_SHM_SLOTS;
    m_pHeader->slot_bytes = FASTRDP_SHM_SLOT_BYTES;
    m_pHeader->data_offset = FASTRDP_SHM_DATA_OFFSET;
    m_pHeader->session_id = sessionId;
    m_pHeader->producer_pid = GetCurrentProcessId();
    InterlockedExchange64(&m_pHeader->latest_frame, -1);
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&m_pHeader->latest_slot), 0);
    MemoryBarrier();
    m_pHeader->magic = FASTRDP_SHM_MAGIC; // last: consumers treat magic as "initialised"
    m_SessionId = sessionId;
    m_failed = false;
    Log(L"FramePublisher: %s ready (%llu MB reserved, existed=%d)", name, total >> 20, err == ERROR_ALREADY_EXISTS ? 1 : 0);
    return true;
}

void FramePublisher::Heartbeat()
{
    if (!m_pHeader) return;
    int64_t now = QpcMicros();
    if (now - m_lastHeartbeat >= 500000) {
        InterlockedExchange64(&m_pHeader->producer_heartbeat_us, now);
        m_lastHeartbeat = now;
    }
}

void FramePublisher::PublishCursor(bool visible, int32_t x, int32_t y, const IDDCX_CURSOR_SHAPE_INFO* info, const uint8_t* shape)
{
    std::lock_guard<std::mutex> g(m_lock);
    if (!m_pHeader) return;
    auto* c = reinterpret_cast<FastRdpShmCursor*>(m_pBase + FASTRDP_SHM_CURSOR_OFFSET);
    InterlockedIncrement64(&c->seq);                      // odd: writing
    c->visible = visible ? 1u : 0u;
    c->x = x;
    c->y = y;
    if (shape && info && info->Width <= FASTRDP_SHM_CURSOR_MAX && info->Height <= FASTRDP_SHM_CURSOR_MAX && info->Pitch >= info->Width * 4) {
        uint8_t* dst = m_pBase + FASTRDP_SHM_CURSOR_SHAPE_OFFSET;
        for (UINT row = 0; row < info->Height; row++)
            memcpy(dst + static_cast<size_t>(row) * info->Width * 4, shape + static_cast<size_t>(row) * info->Pitch, static_cast<size_t>(info->Width) * 4);
        // Our own number, not Windows' shape id: Windows numbers shapes per monitor, so a new monitor
        // or swap-chain in the same session repeats ids for other shapes, and the server only re-reads
        // the shape when the id changes. Counting on from the value in the section keeps it unique for
        // as long as the section exists (also across driver restarts).
        c->shape_id = c->shape_id + 1 != 0 ? c->shape_id + 1 : 1;
        c->shape_type = static_cast<uint32_t>(info->CursorType);
        c->width = info->Width;
        c->height = info->Height;
        c->pitch = info->Width * 4;
        c->x_hot = info->XHot;
        c->y_hot = info->YHot;
    }
    c->updates++;
    InterlockedIncrement64(&c->seq);                      // even: stable
    SetEvent(m_hEvent);
}

static void BeginSlot(FastRdpShmSlot& slot, UINT width, UINT height, UINT stride, uint32_t format, const FramePublisher::SlotInfo& info)
{
    InterlockedIncrement64(&slot.seq);                    // odd: writing
    slot.width = width;
    slot.height = height;
    slot.stride = stride;
    slot.format = format;
    slot.pts_us = info.ptsUs;
    slot.frame_number = info.frameNumber;
    slot.dirty_rects = info.dirtyRects;
}

HRESULT FramePublisher::EnsureConverter(ID3D11Device* dev, UINT width, UINT height)
{
    if (m_CsDevice == dev && m_CsW == width && m_CsH == height) return S_OK;
    HRESULT hr = S_OK;
    if (m_CsDevice != dev) {
        ReleaseConverter();
        // typed UAV stores to R8_UNORM: every feature level 11 device, also WARP
        UINT support = 0;
        if (dev->GetFeatureLevel() < D3D_FEATURE_LEVEL_11_0 || FAILED(dev->CheckFormatSupport(DXGI_FORMAT_R8_UNORM, &support)) ||
            !(support & D3D11_FORMAT_SUPPORT_TYPED_UNORDERED_ACCESS_VIEW)) {
            return DXGI_ERROR_UNSUPPORTED;
        }
        hr = dev->CreateComputeShader(g_ToI420, sizeof(g_ToI420), nullptr, &m_Cs);
        if (FAILED(hr)) return hr;
        D3D11_BUFFER_DESC bd{};
        bd.ByteWidth = 16;
        bd.Usage = D3D11_USAGE_DEFAULT;
        bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
        hr = dev->CreateBuffer(&bd, nullptr, &m_CsSize);
        if (FAILED(hr)) return hr;
        m_CsDevice = dev;
    }
    m_CsStaging.Reset();
    m_CsUav.Reset();
    m_CsDst.Reset();
    m_CsSrv.Reset();
    m_CsSrc.Reset();
    m_CsW = m_CsH = 0;
    D3D11_TEXTURE2D_DESC td{};
    td.Width = width;
    td.Height = height;
    td.MipLevels = 1;
    td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;   // also for _SRGB surfaces: the shader must see the stored bytes
    td.SampleDesc.Count = 1;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    hr = dev->CreateTexture2D(&td, nullptr, &m_CsSrc);
    if (SUCCEEDED(hr)) hr = dev->CreateShaderResourceView(m_CsSrc.Get(), nullptr, &m_CsSrv);
    td.Height = height + height / 2;
    td.Format = DXGI_FORMAT_R8_UNORM;
    td.BindFlags = D3D11_BIND_UNORDERED_ACCESS;
    if (SUCCEEDED(hr)) hr = dev->CreateTexture2D(&td, nullptr, &m_CsDst);
    if (SUCCEEDED(hr)) hr = dev->CreateUnorderedAccessView(m_CsDst.Get(), nullptr, &m_CsUav);
    td.Usage = D3D11_USAGE_STAGING;
    td.BindFlags = 0;
    td.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (SUCCEEDED(hr)) hr = dev->CreateTexture2D(&td, nullptr, &m_CsStaging);
    if (FAILED(hr)) return hr;
    m_CsW = width;
    m_CsH = height;
    Log(L"FramePublisher: GPU I420 conversion %ux%u (1.5 bytes per pixel to the server)", width, height);
    return S_OK;
}

HRESULT FramePublisher::PublishI420(ID3D11DeviceContext* ctx, ID3D11Texture2D* src, UINT width, UINT height, const SlotInfo& info,
                                    FastRdpShmSlot& slot, uint8_t* dst)
{
    const UINT size[4] = { width, height, 0, 0 };
    ctx->UpdateSubresource(m_CsSize.Get(), 0, nullptr, size, 0, 0);
    ctx->CopyResource(m_CsSrc.Get(), src);   // same typeless group (B8G8R8A8), same size
    ID3D11ShaderResourceView* srv = m_CsSrv.Get();
    ID3D11UnorderedAccessView* uav = m_CsUav.Get();
    ID3D11Buffer* cb = m_CsSize.Get();
    ctx->CSSetShader(m_Cs.Get(), nullptr, 0);
    ctx->CSSetShaderResources(0, 1, &srv);
    ctx->CSSetUnorderedAccessViews(0, 1, &uav, nullptr);
    ctx->CSSetConstantBuffers(0, 1, &cb);
    ctx->Dispatch((width / 2 + 7) / 8, (height / 2 + 7) / 8, 1);
    ID3D11ShaderResourceView* noSrv = nullptr;
    ID3D11UnorderedAccessView* noUav = nullptr;
    ctx->CSSetShaderResources(0, 1, &noSrv);
    ctx->CSSetUnorderedAccessViews(0, 1, &noUav, nullptr);
    ctx->CSSetShader(nullptr, nullptr, 0);
    ctx->CopyResource(m_CsStaging.Get(), m_CsDst.Get());
    D3D11_MAPPED_SUBRESOURCE m{};
    HRESULT hr = ctx->Map(m_CsStaging.Get(), 0, D3D11_MAP_READ, 0, &m);
    if (FAILED(hr)) return hr;

    BeginSlot(slot, width, height, width, FASTRDP_SHM_FORMAT_I420, info);
    const uint8_t* rows = static_cast<const uint8_t*>(m.pData);
    const UINT cw = width / 2, ch = height / 2;
    for (UINT y = 0; y < height; y++) memcpy(dst + static_cast<size_t>(y) * width, rows + static_cast<size_t>(y) * m.RowPitch, width);
    uint8_t* u = dst + static_cast<size_t>(width) * height;
    uint8_t* v = u + static_cast<size_t>(cw) * ch;
    for (UINT y = 0; y < ch; y++) {
        const uint8_t* row = rows + static_cast<size_t>(height + y) * m.RowPitch;   // U | V side by side
        memcpy(u + static_cast<size_t>(y) * cw, row, cw);
        memcpy(v + static_cast<size_t>(y) * cw, row + cw, cw);
    }
    ctx->Unmap(m_CsStaging.Get(), 0);
    return S_OK;
}

HRESULT FramePublisher::PublishBgra(ID3D11Device* dev, ID3D11DeviceContext* ctx, ID3D11Texture2D* src, const D3D11_TEXTURE2D_DESC& d,
                                    const SlotInfo& info, FastRdpShmSlot& slot, uint8_t* dst)
{
    HRESULT hr = S_OK;
    if (!m_Staging || m_StagingDevice != dev || m_StagingW != d.Width || m_StagingH != d.Height) {
        D3D11_TEXTURE2D_DESC sd = d;
        sd.Usage = D3D11_USAGE_STAGING;
        sd.BindFlags = 0;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        sd.MiscFlags = 0;
        sd.MipLevels = 1;
        sd.ArraySize = 1;
        m_Staging.Reset();
        hr = dev->CreateTexture2D(&sd, nullptr, &m_Staging);
        if (FAILED(hr)) return hr;
        m_StagingDevice = dev;
        m_StagingW = d.Width;
        m_StagingH = d.Height;
    }
    ctx->CopyResource(m_Staging.Get(), src);
    D3D11_MAPPED_SUBRESOURCE m{};
    hr = ctx->Map(m_Staging.Get(), 0, D3D11_MAP_READ, 0, &m);
    if (FAILED(hr)) return hr;

    BeginSlot(slot, d.Width, d.Height, d.Width * 4, FASTRDP_SHM_FORMAT_BGRA8, info);
    const uint8_t* row = static_cast<const uint8_t*>(m.pData);
    const size_t rowBytes = static_cast<size_t>(d.Width) * 4;
    for (UINT y = 0; y < d.Height; y++, row += m.RowPitch, dst += rowBytes) memcpy(dst, row, rowBytes);
    ctx->Unmap(m_Staging.Get(), 0);
    return S_OK;
}

HRESULT FramePublisher::Publish(ID3D11Device* dev, ID3D11DeviceContext* ctx, IDXGIResource* surface,
                                uint64_t frameNumber, int64_t ptsUs, uint32_t dirtyRects)
{
    std::lock_guard<std::mutex> g(m_lock);
    if (!m_pHeader) return E_NOT_VALID_STATE;
    ComPtr<ID3D11Texture2D> src;
    HRESULT hr = surface->QueryInterface(IID_PPV_ARGS(&src));
    if (FAILED(hr)) return hr;
    D3D11_TEXTURE2D_DESC d{};
    src->GetDesc(&d);
    if (d.Width > FASTRDP_SHM_MAX_WIDTH || d.Height > FASTRDP_SHM_MAX_HEIGHT) return E_INVALIDARG;
    if (d.Format != DXGI_FORMAT_B8G8R8A8_UNORM && d.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) return E_NOTIMPL;

    const SlotInfo info{ frameNumber, ptsUs, dirtyRects };
    int32_t slotIdx = (m_pHeader->latest_slot + 1) % static_cast<int32_t>(FASTRDP_SHM_SLOTS);
    FastRdpShmSlot& slot = m_pHeader->slots[slotIdx];
    uint8_t* dst = m_pBase + m_pHeader->data_offset + static_cast<uint64_t>(slotIdx) * m_pHeader->slot_bytes;

    hr = E_FAIL;
    if (m_CsUnsupported != dev && d.Width % 2 == 0 && d.Height % 2 == 0) {
        hr = EnsureConverter(dev, d.Width, d.Height);
        if (SUCCEEDED(hr)) hr = PublishI420(ctx, src.Get(), d.Width, d.Height, info, slot, dst);
        if (FAILED(hr)) {
            Log(L"FramePublisher: GPU I420 conversion not possible on this device (0x%08X), frames go as BGRA", hr);
            ReleaseConverter();
            m_CsUnsupported = dev;
        }
    }
    if (FAILED(hr)) hr = PublishBgra(dev, ctx, src.Get(), d, info, slot, dst);
    if (FAILED(hr)) return hr;

    InterlockedIncrement64(&slot.seq);                    // even: stable
    InterlockedExchange64(&m_pHeader->latest_frame, static_cast<int64_t>(frameNumber));
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&m_pHeader->latest_slot), slotIdx);
    SetEvent(m_hEvent);
    return S_OK;
}

} // namespace FastRdp
