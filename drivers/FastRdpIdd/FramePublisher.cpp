#include "FramePublisher.h"
#include <sddl.h>

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
    if (m_pBase) UnmapViewOfFile(m_pBase);
    if (m_hSection) CloseHandle(m_hSection);
    if (m_hEvent) CloseHandle(m_hEvent);
    if (m_hCtlEvent) CloseHandle(m_hCtlEvent);
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

void FramePublisher::PublishCursor(bool visible, int32_t x, int32_t y, uint32_t shapeId, const IDDCX_CURSOR_SHAPE_INFO* info, const uint8_t* shape)
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
        c->shape_id = shapeId;
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
    ctx->CopyResource(m_Staging.Get(), src.Get());
    D3D11_MAPPED_SUBRESOURCE m{};
    hr = ctx->Map(m_Staging.Get(), 0, D3D11_MAP_READ, 0, &m);
    if (FAILED(hr)) return hr;

    int32_t slotIdx = (m_pHeader->latest_slot + 1) % static_cast<int32_t>(FASTRDP_SHM_SLOTS);
    FastRdpShmSlot& slot = m_pHeader->slots[slotIdx];
    InterlockedIncrement64(&slot.seq);                    // odd: writing
    slot.width = d.Width;
    slot.height = d.Height;
    slot.stride = d.Width * 4;
    slot.format = FASTRDP_SHM_FORMAT_BGRA8;
    slot.pts_us = ptsUs;
    slot.frame_number = frameNumber;
    slot.dirty_rects = dirtyRects;
    uint8_t* dst = m_pBase + m_pHeader->data_offset + static_cast<uint64_t>(slotIdx) * m_pHeader->slot_bytes;
    const uint8_t* row = static_cast<const uint8_t*>(m.pData);
    const size_t rowBytes = static_cast<size_t>(d.Width) * 4;
    for (UINT y = 0; y < d.Height; y++, row += m.RowPitch, dst += rowBytes) memcpy(dst, row, rowBytes);
    ctx->Unmap(m_Staging.Get(), 0);
    InterlockedIncrement64(&slot.seq);                    // even: stable
    InterlockedExchange64(&m_pHeader->latest_frame, static_cast<int64_t>(frameNumber));
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&m_pHeader->latest_slot), slotIdx);
    SetEvent(m_hEvent);
    return S_OK;
}

} // namespace FastRdp
