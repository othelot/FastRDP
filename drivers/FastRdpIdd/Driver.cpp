/*++
FastRdpIdd - Remote Indirect Display Driver (UMDF 2, IddCx 1.4)

Differences from a console IDD (e.g. Virtual-Display-Driver / IddSampleDriver):
  * Never root-enumerated: the Windows remote desktop stack creates the device per session
    (SWD\RemoteDisplayEnum\FastRDP_RemoteIDD&SessionId_NNNN) and destroys it on disconnect.
  * IDDCX_ADAPTER_CAPS.Flags = REMOTE_SESSION_DRIVER | USE_SMALLEST_MODE (both mandatory;
    IddCxAdapterInitAsync fails if REMOTE is set on a device the RD stack did not create).
  * EDID-less monitor; modes come from a driver table; progressive scan-line ordering only.
  * The remote session's desktop configuration starts EMPTY: after IddCxMonitorArrival the driver
    must call IddCxAdapterDisplayConfigUpdate or nothing becomes active.
  * TransmissionType = OTHER (NETWORK_OTHER is reported rejected by some IddCx builds).

Skeleton: Microsoft Windows-driver-samples video/IndirectDisplay (MIT License).
--*/

#include "Driver.h"
#include "FramePublisher.h"

#include <initguid.h>
#include <devpkey.h>
#include <cstdio>
#include <cstdarg>
#include <mutex>

using namespace Microsoft::WRL;
using namespace FastRdp;

#pragma region Configuration

// Standard modes always offered. The client's resolution (any size, see ProcessControl) is put in
// front at run time; without a client request the session starts at the first entry.
static const ModeEntry s_Modes[] = {
    { 1920, 1080,  60 },
    { 1920, 1080, 120 },
    { 1920, 1080, 144 },
    { 2560, 1440,  60 },
    { 2560, 1440, 120 },
    { 3840, 2160,  60 },
    { 1600,  900,  60 },
    { 1280,  720,  60 },
    { 1024,  768,  60 },
};
static constexpr UINT kPhysicalWidthMm = 527;   // ~24" 16:9, used for DPI calculation
static constexpr UINT kPhysicalHeightMm = 296;

#pragma endregion

#pragma region Logging

namespace FastRdp {

static std::mutex g_logLock;

void Log(_Printf_format_string_ const wchar_t* fmt, ...)
{
    wchar_t msg[1024];
    va_list ap;
    va_start(ap, fmt);
    _vsnwprintf_s(msg, _TRUNCATE, fmt, ap);
    va_end(ap);

    SYSTEMTIME st;
    GetLocalTime(&st);
    wchar_t line[1200];
    _snwprintf_s(line, _TRUNCATE, L"%04u-%02u-%02u %02u:%02u:%02u.%03u pid=%lu tid=%lu %s\r\n",
                 st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds,
                 GetCurrentProcessId(), GetCurrentThreadId(), msg);
    OutputDebugStringW(L"[FastRdpIdd] ");
    OutputDebugStringW(line);

    wchar_t path[MAX_PATH];
    DWORD n = GetEnvironmentVariableW(L"ProgramData", path, MAX_PATH);
    if (n == 0 || n > MAX_PATH - 40) return;
    wcscat_s(path, L"\\FastRDP\\logs\\idd.log");

    char utf8[3600];
    int len = WideCharToMultiByte(CP_UTF8, 0, line, -1, utf8, sizeof(utf8), nullptr, nullptr);
    if (len <= 1) return;

    std::lock_guard<std::mutex> g(g_logLock);
    HANDLE h = CreateFileW(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           nullptr, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (h == INVALID_HANDLE_VALUE) return;
    DWORD written = 0;
    WriteFile(h, utf8, static_cast<DWORD>(len - 1), &written, nullptr);
    CloseHandle(h);
}

} // namespace FastRdp

#pragma endregion

#pragma region Helpers

static void FillSignalInfo(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& Mode, DWORD Width, DWORD Height, DWORD VSync, bool bMonitorMode)
{
    // Indirect displays have no blanking: total == active (IddCxMonitorArrival rejects otherwise).
    Mode.totalSize.cx = Mode.activeSize.cx = Width;
    Mode.totalSize.cy = Mode.activeSize.cy = Height;
    Mode.AdditionalSignalInfo.vSyncFreqDivider = bMonitorMode ? 0 : 1;
    Mode.AdditionalSignalInfo.videoStandard = 255;
    Mode.vSyncFreq.Numerator = VSync;
    Mode.vSyncFreq.Denominator = 1;
    Mode.hSyncFreq.Numerator = VSync * Height;
    Mode.hSyncFreq.Denominator = 1;
    Mode.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE; // mandatory for remote IDDs
    Mode.pixelRate = static_cast<UINT64>(VSync) * Width * Height;
}

bool FastRdp::g_ModernModes = false;

DisplayModes& DisplayModes::Get()
{
    static DisplayModes d;
    static std::once_flag once;
    std::call_once(once, [] {
        d.modes.assign(std::begin(s_Modes), std::end(s_Modes));
        d.current = s_Modes[0];
    });
    return d;
}

std::vector<ModeEntry> DisplayModes::Prefer(const ModeEntry& m)
{
    std::lock_guard<std::mutex> g(lock);
    modes.erase(std::remove_if(modes.begin(), modes.end(), [&](const ModeEntry& e) {
        return e.Width == m.Width && e.Height == m.Height && e.RefreshHz == m.RefreshHz;
    }), modes.end());
    modes.insert(modes.begin(), m);
    if (modes.size() > 32) modes.resize(32);
    current = m;
    return modes;
}

static IDDCX_MONITOR_MODE CreateMonitorMode(const ModeEntry& m)
{
    IDDCX_MONITOR_MODE Mode = {};
    Mode.Size = sizeof(Mode);
    Mode.Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
    FillSignalInfo(Mode.MonitorVideoSignalInfo, m.Width, m.Height, m.RefreshHz, true);
    return Mode;
}

static IDDCX_TARGET_MODE2 CreateTargetMode2(const ModeEntry& m)
{
    IDDCX_TARGET_MODE2 Mode = {};
    Mode.Size = sizeof(Mode);
    FillSignalInfo(Mode.TargetVideoSignalInfo.targetVideoSignalInfo, m.Width, m.Height, m.RefreshHz, false);
    Mode.BitsPerComponent.Rgb = IDDCX_BITS_PER_COMPONENT_8;
    return Mode;
}

static IDDCX_TARGET_MODE CreateTargetMode(const ModeEntry& m)
{
    IDDCX_TARGET_MODE Mode = {};
    Mode.Size = sizeof(Mode);
    FillSignalInfo(Mode.TargetVideoSignalInfo.targetVideoSignalInfo, m.Width, m.Height, m.RefreshHz, false);
    return Mode;
}

// "...\FASTRDP_REMOTEIDD&SESSIONID_0003" -> 3 (decimal, same format as RdpIdd_IndirectDisplay&SessionId_0025)
static ULONG ParseSessionId(const std::wstring& instanceId)
{
    std::wstring up = instanceId;
    for (auto& c : up) c = static_cast<wchar_t>(towupper(c));
    size_t p = up.find(L"SESSIONID_");
    if (p == std::wstring::npos) return 0xFFFFFFFF;
    return wcstoul(up.c_str() + p + 10, nullptr, 10);
}

// Evidence helper: copies a swap-chain surface to a CPU staging texture and writes a 32-bit BMP to
// %ProgramData%\FastRDP\logs\frame-s<session>-<n>.bmp. Debug/evidence only (CPU copy, not the fast path).
static HRESULT DumpFrameToBmp(ID3D11Device* dev, ID3D11DeviceContext* ctx, IDXGIResource* surface, ULONG sessionId, UINT index)
{
    ComPtr<ID3D11Texture2D> src;
    HRESULT hr = surface->QueryInterface(IID_PPV_ARGS(&src));
    if (FAILED(hr)) return hr;
    D3D11_TEXTURE2D_DESC d{};
    src->GetDesc(&d);
    if (d.Format != DXGI_FORMAT_B8G8R8A8_UNORM && d.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) return E_NOTIMPL;
    D3D11_TEXTURE2D_DESC sd = d;
    sd.Usage = D3D11_USAGE_STAGING;
    sd.BindFlags = 0;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    sd.MiscFlags = 0;
    sd.MipLevels = 1;
    sd.ArraySize = 1;
    ComPtr<ID3D11Texture2D> staging;
    hr = dev->CreateTexture2D(&sd, nullptr, &staging);
    if (FAILED(hr)) return hr;
    ctx->CopyResource(staging.Get(), src.Get());
    D3D11_MAPPED_SUBRESOURCE m{};
    hr = ctx->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &m);
    if (FAILED(hr)) return hr;

    wchar_t path[MAX_PATH];
    DWORD n = GetEnvironmentVariableW(L"ProgramData", path, MAX_PATH);
    if (n == 0 || n > MAX_PATH - 64) { ctx->Unmap(staging.Get(), 0); return E_FAIL; }
    wchar_t name[64];
    _snwprintf_s(name, _TRUNCATE, L"\\FastRDP\\logs\\frame-s%lu-%u.bmp", sessionId, index);
    wcscat_s(path, name);

    BITMAPFILEHEADER bf{};
    BITMAPINFOHEADER bi{};
    bi.biSize = sizeof(bi);
    bi.biWidth = static_cast<LONG>(d.Width);
    bi.biHeight = -static_cast<LONG>(d.Height); // top-down
    bi.biPlanes = 1;
    bi.biBitCount = 32;
    bi.biCompression = BI_RGB;
    DWORD rowBytes = d.Width * 4;
    bi.biSizeImage = rowBytes * d.Height;
    bf.bfType = 0x4D42;
    bf.bfOffBits = sizeof(bf) + sizeof(bi);
    bf.bfSize = bf.bfOffBits + bi.biSizeImage;

    HANDLE h = CreateFileW(path, GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (h == INVALID_HANDLE_VALUE) { ctx->Unmap(staging.Get(), 0); return HRESULT_FROM_WIN32(GetLastError()); }
    DWORD w = 0;
    WriteFile(h, &bf, sizeof(bf), &w, nullptr);
    WriteFile(h, &bi, sizeof(bi), &w, nullptr);
    const BYTE* row = static_cast<const BYTE*>(m.pData);
    for (UINT y = 0; y < d.Height; y++, row += m.RowPitch) WriteFile(h, row, rowBytes, &w, nullptr);
    CloseHandle(h);
    ctx->Unmap(staging.Get(), 0);
    Log(L"frame dump -> %s", path);
    return S_OK;
}

#pragma endregion

#pragma region WDF context wrappers

struct IndirectDeviceContextWrapper {
    IndirectDeviceContext* pContext;
    void Cleanup() { delete pContext; pContext = nullptr; }
};

struct IndirectMonitorContextWrapper {
    IndirectMonitorContext* pContext;
    void Cleanup() { delete pContext; pContext = nullptr; }
};

WDF_DECLARE_CONTEXT_TYPE(IndirectDeviceContextWrapper);
WDF_DECLARE_CONTEXT_TYPE(IndirectMonitorContextWrapper);

#pragma endregion

extern "C" DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD FastRdpIddDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY FastRdpIddDeviceD0Entry;
EVT_IDD_CX_ADAPTER_INIT_FINISHED FastRdpIddAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES FastRdpIddAdapterCommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION FastRdpIddParseMonitorDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES FastRdpIddMonitorGetDefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES FastRdpIddMonitorQueryModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN FastRdpIddMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN FastRdpIddMonitorUnassignSwapChain;
EVT_IDD_CX_MONITOR_GET_PHYSICAL_SIZE FastRdpIddMonitorGetPhysicalSize;
// IddCx 1.10
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION2 FastRdpIddParseMonitorDescription2;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES2 FastRdpIddMonitorQueryModes2;
EVT_IDD_CX_ADAPTER_COMMIT_MODES2 FastRdpIddAdapterCommitModes2;
EVT_IDD_CX_ADAPTER_QUERY_TARGET_INFO FastRdpIddAdapterQueryTargetInfo;
EVT_IDD_CX_MONITOR_SET_DEFAULT_HDR_METADATA FastRdpIddMonitorSetDefaultHdrMetaData;
EVT_IDD_CX_MONITOR_SET_GAMMA_RAMP FastRdpIddMonitorSetGammaRamp;

extern "C" BOOL WINAPI DllMain(_In_ HINSTANCE, _In_ UINT, _In_opt_ LPVOID)
{
    return TRUE;
}

_Use_decl_annotations_
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT pDriverObject, PUNICODE_STRING pRegistryPath)
{
    Log(L"DriverEntry (FastRdpIdd, IddCx %u.%u)", IDDCX_VERSION_MAJOR, IDDCX_VERSION_MINOR);
    WDF_DRIVER_CONFIG Config;
    WDF_OBJECT_ATTRIBUTES Attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&Attributes);
    WDF_DRIVER_CONFIG_INIT(&Config, FastRdpIddDeviceAdd);
    NTSTATUS Status = WdfDriverCreate(pDriverObject, pRegistryPath, &Attributes, &Config, WDF_NO_HANDLE);
    Log(L"WdfDriverCreate -> 0x%08X", Status);
    return Status;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddDeviceAdd(WDFDRIVER Driver, PWDFDEVICE_INIT pDeviceInit)
{
    UNREFERENCED_PARAMETER(Driver);
    Log(L"DeviceAdd: entered");

    WDF_PNPPOWER_EVENT_CALLBACKS PnpPowerCallbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&PnpPowerCallbacks);
    PnpPowerCallbacks.EvtDeviceD0Entry = FastRdpIddDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(pDeviceInit, &PnpPowerCallbacks);

    IDD_CX_CLIENT_CONFIG IddConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&IddConfig);
    IddConfig.EvtIddCxAdapterInitFinished = FastRdpIddAdapterInitFinished;
    IddConfig.EvtIddCxMonitorGetDefaultDescriptionModes = FastRdpIddMonitorGetDefaultModes;
    g_ModernModes = IDD_IS_FUNCTION_AVAILABLE(IddCxMonitorUpdateModes2) && IDD_IS_FIELD_AVAILABLE(IDD_CX_CLIENT_CONFIG, EvtIddCxMonitorQueryTargetModes2);
    if (g_ModernModes) {
        // Same DDI set as the inbox RdpIdd (8-bit SDR only).
        IddConfig.EvtIddCxParseMonitorDescription2 = FastRdpIddParseMonitorDescription2;
        IddConfig.EvtIddCxMonitorQueryTargetModes2 = FastRdpIddMonitorQueryModes2;
        IddConfig.EvtIddCxAdapterCommitModes2 = FastRdpIddAdapterCommitModes2;
        IddConfig.EvtIddCxAdapterQueryTargetInfo = FastRdpIddAdapterQueryTargetInfo;
        IddConfig.EvtIddCxMonitorSetDefaultHdrMetaData = FastRdpIddMonitorSetDefaultHdrMetaData;
        IddConfig.EvtIddCxMonitorSetGammaRamp = FastRdpIddMonitorSetGammaRamp;
    } else {
        IddConfig.EvtIddCxParseMonitorDescription = FastRdpIddParseMonitorDescription;
        IddConfig.EvtIddCxMonitorQueryTargetModes = FastRdpIddMonitorQueryModes;
        IddConfig.EvtIddCxAdapterCommitModes = FastRdpIddAdapterCommitModes;
    }
    Log(L"IddCx mode DDIs: %s", g_ModernModes ? L"1.10 (*2, all target modes monitor-compatible)" : L"1.4 legacy (fixed monitor mode list)");
    IddConfig.EvtIddCxMonitorAssignSwapChain = FastRdpIddMonitorAssignSwapChain;
    IddConfig.EvtIddCxMonitorUnassignSwapChain = FastRdpIddMonitorUnassignSwapChain;
    IddConfig.EvtIddCxMonitorGetPhysicalSize = FastRdpIddMonitorGetPhysicalSize;

    NTSTATUS Status = IddCxDeviceInitConfig(pDeviceInit, &IddConfig);
    Log(L"IddCxDeviceInitConfig -> 0x%08X", Status);
    if (!NT_SUCCESS(Status)) return Status;

    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, IndirectDeviceContextWrapper);
    Attr.EvtCleanupCallback = [](WDFOBJECT Object) {
        auto* p = WdfObjectGet_IndirectDeviceContextWrapper(Object);
        if (p) p->Cleanup();
    };

    WDFDEVICE Device = nullptr;
    Status = WdfDeviceCreate(&pDeviceInit, &Attr, &Device);
    Log(L"WdfDeviceCreate -> 0x%08X", Status);
    if (!NT_SUCCESS(Status)) return Status;

    Status = IddCxDeviceInitialize(Device);
    Log(L"IddCxDeviceInitialize -> 0x%08X", Status);

    auto* pContext = WdfObjectGet_IndirectDeviceContextWrapper(Device);
    pContext->pContext = new IndirectDeviceContext(Device);
    return Status;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddDeviceD0Entry(WDFDEVICE Device, WDF_POWER_DEVICE_STATE PreviousState)
{
    Log(L"D0Entry (previous state %d)", PreviousState);
    auto* pContext = WdfObjectGet_IndirectDeviceContextWrapper(Device);
    pContext->pContext->InitAdapter();
    return STATUS_SUCCESS;
}

#pragma region Direct3DDevice

HRESULT Direct3DDevice::Init()
{
    HRESULT hr = CreateDXGIFactory2(0, IID_PPV_ARGS(&DxgiFactory));
    if (FAILED(hr)) return hr;
    hr = DxgiFactory->EnumAdapterByLuid(AdapterLuid, IID_PPV_ARGS(&Adapter));
    if (FAILED(hr)) return hr;
    DXGI_ADAPTER_DESC1 desc{};
    if (SUCCEEDED(Adapter->GetDesc1(&desc))) {
        wchar_t buf[200];
        _snwprintf_s(buf, _TRUNCATE, L"%s (VendorId=0x%04X DeviceId=0x%04X)", desc.Description, desc.VendorId, desc.DeviceId);
        AdapterDescription = buf;
    }
    // BGRA support is required by the IddCx/WHQL rules.
    hr = D3D11CreateDevice(Adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                           nullptr, 0, D3D11_SDK_VERSION, &Device, nullptr, &DeviceContext);
    return hr;
}

#pragma endregion

#pragma region SwapChainProcessor

SwapChainProcessor::SwapChainProcessor(IDDCX_MONITOR Monitor, IDDCX_SWAPCHAIN hSwapChain, std::shared_ptr<Direct3DDevice> Device, HANDLE NewFrameEvent, ULONG SessionId)
    : m_Monitor(Monitor), m_hSwapChain(hSwapChain), m_Device(std::move(Device)), m_hAvailableBufferEvent(NewFrameEvent), m_SessionId(SessionId)
{
    m_StartTick = GetTickCount64();
    m_hTerminateEvent.Attach(CreateEvent(nullptr, TRUE, FALSE, nullptr)); // manual-reset: also polled after RunCore
    m_hCursorEvent.Attach(CreateEvent(nullptr, FALSE, FALSE, nullptr));
    m_hCursorStop.Attach(CreateEvent(nullptr, TRUE, FALSE, nullptr));
    m_hThread.Attach(CreateThread(nullptr, 0, RunThread, this, 0, nullptr));
}

SwapChainProcessor::~SwapChainProcessor()
{
    SetEvent(m_hTerminateEvent.Get());
    if (m_hThread.Get()) WaitForSingleObject(m_hThread.Get(), INFINITE);
    SetEvent(m_hCursorStop.Get());
    if (m_hCursorThread.Get()) WaitForSingleObject(m_hCursorThread.Get(), INFINITE);
}

void SwapChainProcessor::SetupCursor()
{
    if (m_hCursorThread.Get()) return;
    IDARG_IN_SETUP_HWCURSOR In = {};
    In.CursorInfo.Size = sizeof(In.CursorInfo);
    In.CursorInfo.ColorXorCursorSupport = IDDCX_XOR_CURSOR_SUPPORT_FULL; // the client renders XOR pixels itself
    In.CursorInfo.AlphaCursorSupport = TRUE;
    In.CursorInfo.MaxX = FASTRDP_SHM_CURSOR_MAX;
    In.CursorInfo.MaxY = FASTRDP_SHM_CURSOR_MAX;
    In.hNewCursorDataAvailable = m_hCursorEvent.Get();
    NTSTATUS st = IddCxMonitorSetupHardwareCursor(m_Monitor, &In);
    Log(L"IddCxMonitorSetupHardwareCursor(max %u, xor full, alpha) -> 0x%08X", FASTRDP_SHM_CURSOR_MAX, st);
    if (NT_SUCCESS(st)) m_hCursorThread.Attach(CreateThread(nullptr, 0, CursorThread, this, 0, nullptr));
}

DWORD CALLBACK SwapChainProcessor::CursorThread(LPVOID Argument)
{
    reinterpret_cast<SwapChainProcessor*>(Argument)->CursorLoop();
    return 0;
}

void SwapChainProcessor::CursorLoop()
{
    std::vector<BYTE> shape(FASTRDP_SHM_CURSOR_SHAPE_BYTES);
    DWORD lastShapeId = 0;
    UINT64 queries = 0, failures = 0, shapes = 0;
    for (;;) {
        HANDLE h[] = { m_hCursorEvent.Get(), m_hCursorStop.Get() };
        DWORD w = WaitForMultipleObjects(ARRAYSIZE(h), h, FALSE, INFINITE);
        if (w != WAIT_OBJECT_0) break;
        IDARG_IN_QUERY_HWCURSOR In = {};
        In.LastShapeId = lastShapeId;
        In.ShapeBufferSizeInBytes = static_cast<UINT>(shape.size());
        In.pShapeBuffer = shape.data();
        IDARG_OUT_QUERY_HWCURSOR Out = {};
        NTSTATUS st = IddCxMonitorQueryHardwareCursor(m_Monitor, &In, &Out);
        ++queries;
        if (!NT_SUCCESS(st)) {
            if (failures++ < 5) Log(L"IddCxMonitorQueryHardwareCursor -> 0x%08X", st);
            continue;
        }
        const bool updated = Out.IsCursorShapeUpdated && Out.CursorShapeInfo.Width > 0;
        if (updated) {
            lastShapeId = Out.CursorShapeInfo.ShapeId;
            if (shapes++ < 20)
                Log(L"cursor shape %u type=%u %ux%u pitch=%u hot=%u,%u", lastShapeId, Out.CursorShapeInfo.CursorType,
                    Out.CursorShapeInfo.Width, Out.CursorShapeInfo.Height, Out.CursorShapeInfo.Pitch,
                    Out.CursorShapeInfo.XHot, Out.CursorShapeInfo.YHot);
        }
        FramePublisher::Instance().PublishCursor(Out.IsCursorVisible != FALSE, Out.X, Out.Y, lastShapeId,
                                                 updated ? &Out.CursorShapeInfo : nullptr, updated ? shape.data() : nullptr);
    }
    Log(L"cursor thread stopped after %llu queries (%llu shapes)", queries, shapes);
}

DWORD CALLBACK SwapChainProcessor::RunThread(LPVOID Argument)
{
    reinterpret_cast<SwapChainProcessor*>(Argument)->Run();
    return 0;
}

void SwapChainProcessor::Run()
{
    DWORD AvTask = 0;
    HANDLE AvTaskHandle = AvSetMmThreadCharacteristicsW(L"Distribution", &AvTask);
    bool lost = RunCore();
    SetEvent(m_hCursorStop.Get()); // the hardware-cursor setup belongs to this swap-chain
    // GPU objects made on this swap-chain's device go now, on this thread (see FramePublisher::ReleaseGpu)
    FramePublisher::Instance().ReleaseGpu(m_Device->Device.Get());
    if (lost && WaitForSingleObject(m_hTerminateEvent.Get(), 0) == WAIT_OBJECT_0) {
        // IddCx is already unassigning this swap-chain (e.g. ACCESS_LOST during re-assignment): it owns it.
        lost = false;
    }
    if (lost) {
        // Our own failure / ACCESS_LOST: delete so IddCx assigns a fresh swap-chain.
        // When IddCx stopped us (Unassign/terminate) it owns the swap-chain; deleting it from here has
        // been reported to deadlock device removal (OpenStreamMultiSeat notes), so we do not.
        Log(L"SwapChain: lost/failed -> WdfObjectDelete(swapchain)");
        WdfObjectDelete(reinterpret_cast<WDFOBJECT>(m_hSwapChain));
    } else {
        Log(L"SwapChain: processing thread stopped by IddCx");
    }
    m_hSwapChain = nullptr;
    if (AvTaskHandle) AvRevertMmThreadCharacteristics(AvTaskHandle);
}

bool SwapChainProcessor::RunCore()
{
    ComPtr<IDXGIDevice> DxgiDevice;
    HRESULT hr = m_Device->Device.As(&DxgiDevice);
    if (FAILED(hr)) { Log(L"SwapChain: QI IDXGIDevice failed 0x%08X", hr); return true; }

    IDARG_IN_SWAPCHAINSETDEVICE SetDevice = {};
    SetDevice.pDevice = DxgiDevice.Get();
    hr = IddCxSwapChainSetDevice(m_hSwapChain, &SetDevice);
    Log(L"IddCxSwapChainSetDevice -> 0x%08X", hr);
    if (FAILED(hr)) return true;

    UINT64 frames = 0;
    UINT64 windowFrames = 0;
    ULONGLONG windowStart = GetTickCount64();
    bool firstLogged = false;

    UINT dumps = 0;
    ULONGLONG lastDump = 0;
    bool publisherReady = FramePublisher::Instance().Ensure(m_SessionId);
    if (publisherReady) SetupCursor();
    UINT publishErrors = 0;
    for (;;) {
        ULONGLONG now = GetTickCount64();
        if (now - windowStart >= 5000) {
            Log(L"frames: session=%lu total=%llu last%.1fs=%llu (%.1f fps)", m_SessionId, frames,
                (now - windowStart) / 1000.0, windowFrames, windowFrames * 1000.0 / static_cast<double>(now - windowStart));
            windowFrames = 0;
            windowStart = now;
        }
        if (publisherReady) FramePublisher::Instance().Heartbeat();
        IDARG_OUT_RELEASEANDACQUIREBUFFER Buffer = {};
        hr = IddCxSwapChainReleaseAndAcquireBuffer(m_hSwapChain, &Buffer);
        const int64_t acquiredUs = QpcMicros();

        if (hr == E_PENDING) {
            HANDLE WaitHandles[] = { m_hAvailableBufferEvent, m_hTerminateEvent.Get() };
            DWORD WaitResult = WaitForMultipleObjects(ARRAYSIZE(WaitHandles), WaitHandles, FALSE, 16);
            if (WaitResult == WAIT_OBJECT_0 || WaitResult == WAIT_TIMEOUT) continue;
            if (WaitResult == WAIT_OBJECT_0 + 1) {
                Log(L"SwapChain: terminate requested after %llu frames", frames);
                return false;
            }
            Log(L"SwapChain: wait failed %lu", WaitResult);
            return true;
        }
        if (FAILED(hr)) {
            Log(L"IddCxSwapChainReleaseAndAcquireBuffer -> 0x%08X after %llu frames (swap-chain abandoned)", hr, frames);
            return true;
        }

        ComPtr<IDXGIResource> AcquiredBuffer;
        AcquiredBuffer.Attach(Buffer.MetaData.pSurface);
        ++frames;
        ++windowFrames;

        if (!firstLogged) {
            ComPtr<ID3D11Texture2D> tex;
            D3D11_TEXTURE2D_DESC d{};
            if (SUCCEEDED(AcquiredBuffer.As(&tex))) tex->GetDesc(&d);
            Log(L"FIRST FRAME session=%lu %ux%u format=%d presentationFrame=%llu dirtyRects=%u",
                m_SessionId, d.Width, d.Height, static_cast<int>(d.Format), static_cast<unsigned long long>(Buffer.MetaData.PresentationFrameNumber),
                Buffer.MetaData.DirtyRectCount);
            firstLogged = true;
        }

        // Evidence: dump frame #1 (after 3 s), then one frame every 20 s, max 5 files per swap-chain.
        ULONGLONG t = GetTickCount64();
        if (dumps < 5 && (dumps == 0 ? (frames > 1 && t >= m_StartTick + 3000)
                                     : (t - lastDump >= 20000))) {
            HRESULT dh = DumpFrameToBmp(m_Device->Device.Get(), m_Device->DeviceContext.Get(), AcquiredBuffer.Get(), m_SessionId, dumps);
            if (FAILED(dh)) Log(L"frame dump failed 0x%08X", dh);
            ++dumps;
            lastDump = t;
        }

        // CPU path: publish into the per-session shared memory for fastrdp-server (x264/OpenH264).
        // Future: GPU shared texture -> hardware encoder (no CPU copy) next to this.
        if (publisherReady) {
            HRESULT ph = FramePublisher::Instance().Publish(m_Device->Device.Get(), m_Device->DeviceContext.Get(),
                                                            AcquiredBuffer.Get(), frames, acquiredUs,
                                                            Buffer.MetaData.DirtyRectCount);
            if (FAILED(ph) && publishErrors++ < 5) Log(L"FramePublisher::Publish -> 0x%08X", ph);
        }
        AcquiredBuffer.Reset();
        hr = IddCxSwapChainFinishedProcessingFrame(m_hSwapChain);
        if (FAILED(hr)) {
            Log(L"IddCxSwapChainFinishedProcessingFrame -> 0x%08X", hr);
            return true;
        }

    }
}

#pragma endregion

#pragma region IndirectDeviceContext

IndirectDeviceContext::IndirectDeviceContext(_In_ WDFDEVICE WdfDevice) : m_WdfDevice(WdfDevice)
{
    WDF_DEVICE_PROPERTY_DATA prop;
    WDF_DEVICE_PROPERTY_DATA_INIT(&prop, &DEVPKEY_Device_InstanceId);
    WDFMEMORY mem = nullptr;
    DEVPROPTYPE type = 0;
    NTSTATUS s = WdfDeviceAllocAndQueryPropertyEx(WdfDevice, &prop, NonPagedPoolNx, WDF_NO_OBJECT_ATTRIBUTES, &mem, &type);
    if (NT_SUCCESS(s) && mem) {
        size_t cb = 0;
        auto* p = static_cast<const wchar_t*>(WdfMemoryGetBuffer(mem, &cb));
        m_InstanceId.assign(p, wcsnlen(p, cb / sizeof(wchar_t)));
        WdfObjectDelete(mem);
    }
    m_SessionId = ParseSessionId(m_InstanceId);
    Log(L"Device instance '%s' -> sessionId=%lu (query 0x%08X)", m_InstanceId.c_str(), m_SessionId, s);
}

IndirectDeviceContext::~IndirectDeviceContext()
{
    if (m_hStopControl.IsValid()) SetEvent(m_hStopControl.Get());
    if (m_hControlThread.IsValid()) WaitForSingleObject(m_hControlThread.Get(), INFINITE);
    Log(L"Device context destroyed (session %lu)", m_SessionId);
}

void IndirectDeviceContext::InitAdapter()
{
    IDDCX_ADAPTER_CAPS AdapterCaps = {};
    AdapterCaps.Size = sizeof(AdapterCaps);
    AdapterCaps.Flags = static_cast<IDDCX_ADAPTER_FLAGS>(IDDCX_ADAPTER_FLAGS_REMOTE_SESSION_DRIVER | IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE |
        (g_ModernModes ? IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE : 0));
    AdapterCaps.MaxMonitorsSupported = 1;

    AdapterCaps.EndPointDiagnostics.Size = sizeof(AdapterCaps.EndPointDiagnostics);
    AdapterCaps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    AdapterCaps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_OTHER;
    AdapterCaps.EndPointDiagnostics.pEndPointFriendlyName = L"FastRDP Remote Display";
    AdapterCaps.EndPointDiagnostics.pEndPointManufacturerName = L"FastRDP";
    AdapterCaps.EndPointDiagnostics.pEndPointModelName = L"FastRDP Remote IDD";

    // Required: IddCxAdapterInitAsync returns STATUS_INVALID_PARAMETER without these.
    static IDDCX_ENDPOINT_VERSION Version = { sizeof(IDDCX_ENDPOINT_VERSION), 1, 0, 0, 0 };
    AdapterCaps.EndPointDiagnostics.pFirmwareVersion = &Version;
    AdapterCaps.EndPointDiagnostics.pHardwareVersion = &Version;

    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, IndirectDeviceContextWrapper);

    IDARG_IN_ADAPTER_INIT AdapterInit = {};
    AdapterInit.WdfDevice = m_WdfDevice;
    AdapterInit.pCaps = &AdapterCaps;
    AdapterInit.ObjectAttributes = &Attr;

    IDARG_OUT_ADAPTER_INIT AdapterInitOut = {};
    NTSTATUS Status = IddCxAdapterInitAsync(&AdapterInit, &AdapterInitOut);
    Log(L"IddCxAdapterInitAsync(flags=0x%X) -> 0x%08X", AdapterCaps.Flags, Status);
    if (NT_SUCCESS(Status)) {
        m_Adapter = AdapterInitOut.AdapterObject;
        auto* pContext = WdfObjectGet_IndirectDeviceContextWrapper(AdapterInitOut.AdapterObject);
        pContext->pContext = this;
    }
}

void IndirectDeviceContext::FinishInit()
{
    WDF_OBJECT_ATTRIBUTES Attr;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&Attr, IndirectMonitorContextWrapper);

    IDDCX_MONITOR_INFO MonitorInfo = {};
    MonitorInfo.Size = sizeof(MonitorInfo);
    // INDIRECT_VIRTUAL is rejected by IddCxMonitorCreate (0xC000000D, observed 2026-11-11); INDIRECT_WIRED works.
    MonitorInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED;
    MonitorInfo.ConnectorIndex = 0;
    MonitorInfo.MonitorDescription.Size = sizeof(MonitorInfo.MonitorDescription);
    MonitorInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID; // EDID type with zero size = no descriptor
    MonitorInfo.MonitorDescription.DataSize = 0;
    MonitorInfo.MonitorDescription.pData = nullptr;
    CoCreateGuid(&MonitorInfo.MonitorContainerId);

    IDARG_IN_MONITORCREATE MonitorCreate = {};
    MonitorCreate.ObjectAttributes = &Attr;
    MonitorCreate.pMonitorInfo = &MonitorInfo;

    IDARG_OUT_MONITORCREATE MonitorCreateOut = {};
    NTSTATUS Status = IddCxMonitorCreate(m_Adapter, &MonitorCreate, &MonitorCreateOut);
    Log(L"IddCxMonitorCreate -> 0x%08X", Status);
    if (!NT_SUCCESS(Status)) return;

    m_Monitor = MonitorCreateOut.MonitorObject;
    auto* pWrapper = WdfObjectGet_IndirectMonitorContextWrapper(m_Monitor);
    pWrapper->pContext = new IndirectMonitorContext(m_Monitor, m_SessionId);

    TakeInitialRequest();
    IDARG_OUT_MONITORARRIVAL ArrivalOut = {};
    Status = IddCxMonitorArrival(m_Monitor, &ArrivalOut);
    Log(L"IddCxMonitorArrival -> 0x%08X (adapter LUID %08X:%08X, target %u)", Status,
        ArrivalOut.OsAdapterLuid.HighPart, ArrivalOut.OsAdapterLuid.LowPart, ArrivalOut.OsTargetId);
    if (!NT_SUCCESS(Status)) return;
    ProcessControl(true);
    m_hStopControl.Attach(CreateEvent(nullptr, TRUE, FALSE, nullptr));
    m_hControlThread.Attach(CreateThread(nullptr, 0, ControlThread, this, 0, nullptr));
}

DWORD CALLBACK IndirectDeviceContext::ControlThread(LPVOID Argument)
{
    reinterpret_cast<IndirectDeviceContext*>(Argument)->ControlLoop();
    return 0;
}

void IndirectDeviceContext::ControlLoop()
{
    for (;;) {
        HANDLE ctl = FramePublisher::Instance().ControlEvent();
        HANDLE h[] = { m_hStopControl.Get(), ctl ? ctl : m_hStopControl.Get() };
        DWORD w = WaitForMultipleObjects(ctl ? 2 : 1, h, FALSE, 1000);
        if (w == WAIT_OBJECT_0) break;
        ProcessControl(false);
    }
    Log(L"display control thread stopped (session %lu)", m_SessionId);
}

// A client display request clamped to what the frame shared memory and the encoders accept.
static void ModeFromRequest(const FastRdpShmControl& c, ModeEntry& mode, UINT& scale)
{
    UINT w = std::min<UINT>(std::max<UINT>(c.width, 640), FASTRDP_SHM_MAX_WIDTH) & ~7u;
    UINT h = std::min<UINT>(std::max<UINT>(c.height, 480), FASTRDP_SHM_MAX_HEIGHT) & ~1u;
    UINT hz = (c.refresh_hz >= 24 && c.refresh_hz <= 240) ? c.refresh_hz : 60;
    mode = { w, h, hz };
    scale = (c.scale_percent >= 100 && c.scale_percent <= 500) ? c.scale_percent : 100;
}

// The OS asks for the monitor's target modes during IddCxMonitorArrival and the initial
// IddCxAdapterDisplayConfigUpdate is rejected (0xC000000D) for a mode that was not in that list -
// the session then has no active path and no frames until the next (change) request. Observed
// 2026-11-12 with macOS clients (1728x1116, 3456x2160); standard sizes were in the default list.
void IndirectDeviceContext::TakeInitialRequest()
{
    auto& pub = FramePublisher::Instance();
    pub.Ensure(m_SessionId);   // the server may have created the section already
    FastRdpShmControl c{};
    if (!pub.ReadControl(c)) return;
    ModeEntry mode;
    UINT scale;
    ModeFromRequest(c, mode, scale);
    auto& dm = DisplayModes::Get();
    dm.Prefer(mode);
    {
        std::lock_guard<std::mutex> g(dm.lock);
        dm.scale = scale;
    }
    m_InitialRequestId = c.request_id;
    Log(L"display request #%u %ux%u@%u scale %u%% taken before monitor arrival", c.request_id, mode.Width, mode.Height, mode.RefreshHz, scale);
}

// Initial call (after monitor arrival): always applies a configuration - the client's request
// taken by TakeInitialRequest, else the default mode. Later calls: only new requests.
void IndirectDeviceContext::ProcessControl(bool initial)
{
    auto& pub = FramePublisher::Instance();
    FastRdpShmControl c{};
    bool pending = initial ? m_InitialRequestId != 0 : pub.ReadControl(c);
    if (initial) c.request_id = m_InitialRequestId;
    if (!pending && !initial) return;
    auto& dm = DisplayModes::Get();
    ModeEntry mode;
    UINT scale;
    {
        std::lock_guard<std::mutex> g(dm.lock);
        mode = dm.current;   // initial: the taken request (TakeInitialRequest) or the default
        scale = dm.scale;
    }
    if (pending && !initial) ModeFromRequest(c, mode, scale);
    std::vector<ModeEntry> list = dm.Prefer(mode);
    {
        std::lock_guard<std::mutex> g(dm.lock);
        dm.scale = scale;
    }
    NTSTATUS st = STATUS_SUCCESS;
    if (!initial && g_ModernModes) {
        std::vector<IDDCX_TARGET_MODE2> targets;
        for (const auto& m : list) targets.push_back(CreateTargetMode2(m));
        IDARG_IN_UPDATEMODES2 um = {};
        um.Reason = IDDCX_UPDATE_REASON_OTHER;
        um.TargetModeCount = static_cast<UINT>(targets.size());
        um.pTargetModes = targets.data();
        st = IddCxMonitorUpdateModes2(m_Monitor, &um);
        Log(L"IddCxMonitorUpdateModes2(%u modes, preferred %ux%u@%u) -> 0x%08X", um.TargetModeCount, mode.Width, mode.Height, mode.RefreshHz, st);
    } else if (!initial) {
        std::vector<IDDCX_TARGET_MODE> targets;
        for (const auto& m : list) targets.push_back(CreateTargetMode(m));
        IDARG_IN_UPDATEMODES um = {};
        um.Reason = IDDCX_UPDATE_REASON_OTHER;
        um.TargetModeCount = static_cast<UINT>(targets.size());
        um.pTargetModes = targets.data();
        st = IddCxMonitorUpdateModes(m_Monitor, &um);
        Log(L"IddCxMonitorUpdateModes(%u modes, preferred %ux%u@%u) -> 0x%08X (legacy: only sizes in the monitor's default list can be applied)", um.TargetModeCount, mode.Width, mode.Height, mode.RefreshHz, st);
    }
    if (NT_SUCCESS(st)) st = ApplyDisplayConfig(mode, scale);
    if (pending) pub.ReportControl(c.request_id, st, mode.Width, mode.Height, scale);
    Log(L"display request #%u %ux%u@%u scale %u%% (%s) -> 0x%08X", pending ? c.request_id : 0, mode.Width, mode.Height,
        mode.RefreshHz, scale, initial ? L"initial" : L"change", st);
}

NTSTATUS IndirectDeviceContext::ApplyDisplayConfig(const ModeEntry& m, UINT scalePercent)
{
    // The OS keeps one desktop configuration per remote session and it starts EMPTY; without this
    // call the arrived monitor never becomes an active path.
    IDDCX_DISPLAYCONFIGPATH Path = {};
    Path.Size = sizeof(Path);
    Path.MonitorObject = m_Monitor;
    Path.Position = { 0, 0 };
    Path.Resolution = { m.Width, m.Height };
    Path.Rotation = DISPLAYCONFIG_ROTATION_IDENTITY;
    Path.RefreshRate = { m.RefreshHz, 1 };
    Path.VSyncFreqDivider = 1;
    Path.MonitorScaleFactor = scalePercent;

    IDARG_IN_ADAPTERDISPLAYCONFIGUPDATE In = {};
    In.PathCount = 1;
    In.pPaths = &Path;
    NTSTATUS Status = IddCxAdapterDisplayConfigUpdate(m_Adapter, &In);
    // STATUS_GRAPHICS_INDIRECT_DISPLAY_DEVICE_STOPPED is expected while disconnecting; never treat as fatal.
    Log(L"IddCxAdapterDisplayConfigUpdate(%ux%u@%u, scale %u%%) -> 0x%08X", m.Width, m.Height, m.RefreshHz, scalePercent, Status);
    return Status;
}

IndirectMonitorContext::IndirectMonitorContext(_In_ IDDCX_MONITOR Monitor, ULONG SessionId)
    : m_Monitor(Monitor), m_SessionId(SessionId)
{
}

IndirectMonitorContext::~IndirectMonitorContext()
{
    m_ProcessingThread.reset();
}

void IndirectMonitorContext::AssignSwapChain(IDDCX_SWAPCHAIN SwapChain, LUID RenderAdapter, HANDLE NewFrameEvent)
{
    m_ProcessingThread.reset();
    auto Device = std::make_shared<Direct3DDevice>(RenderAdapter);
    HRESULT hr = Device->Init();
    Log(L"AssignSwapChain: render adapter LUID %08X:%08X '%s' D3D11 init -> 0x%08X",
        RenderAdapter.HighPart, RenderAdapter.LowPart, Device->AdapterDescription.c_str(), hr);
    if (FAILED(hr)) {
        // Delete so the OS generates a new swap-chain and retries.
        WdfObjectDelete(SwapChain);
        return;
    }
    m_ProcessingThread.reset(new SwapChainProcessor(m_Monitor, SwapChain, Device, NewFrameEvent, m_SessionId));
}

void IndirectMonitorContext::UnassignSwapChain()
{
    Log(L"UnassignSwapChain (session %lu)", m_SessionId);
    m_ProcessingThread.reset();
}

#pragma endregion

#pragma region DDI callbacks

_Use_decl_annotations_
NTSTATUS FastRdpIddAdapterInitFinished(IDDCX_ADAPTER AdapterObject, const IDARG_IN_ADAPTER_INIT_FINISHED* pInArgs)
{
    Log(L"AdapterInitFinished -> AdapterInitStatus=0x%08X", pInArgs->AdapterInitStatus);
    auto* pWrapper = WdfObjectGet_IndirectDeviceContextWrapper(AdapterObject);
    if (NT_SUCCESS(pInArgs->AdapterInitStatus)) pWrapper->pContext->FinishInit();
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddAdapterCommitModes(IDDCX_ADAPTER AdapterObject, const IDARG_IN_COMMITMODES* pInArgs)
{
    UNREFERENCED_PARAMETER(AdapterObject);
    Log(L"AdapterCommitModes: %u path(s)", pInArgs->PathCount);
    for (UINT i = 0; i < pInArgs->PathCount; i++) {
        const auto& p = pInArgs->pPaths[i];
        const auto& s = p.TargetVideoSignalInfo;
        Log(L"  path %u flags=0x%X active=%d %ux%u @ %u/%u", i, p.Flags, (p.Flags & IDDCX_PATH_FLAGS_ACTIVE) ? 1 : 0,
            s.activeSize.cx, s.activeSize.cy, s.vSyncFreq.Numerator, s.vSyncFreq.Denominator);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* pInArgs, IDARG_OUT_PARSEMONITORDESCRIPTION* pOutArgs)
{
    // We never report an EDID, so there is nothing to parse.
    Log(L"ParseMonitorDescription (descriptor size %u) -> 0 modes", pInArgs->MonitorDescription.DataSize);
    pOutArgs->MonitorModeBufferOutputCount = 0;
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorGetDefaultModes(IDDCX_MONITOR MonitorObject, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* pInArgs, IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* pOutArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    auto& dm = DisplayModes::Get();
    std::lock_guard<std::mutex> g(dm.lock);
    const UINT n = static_cast<UINT>(dm.modes.size());
    pOutArgs->DefaultMonitorModeBufferOutputCount = n;
    if (pInArgs->DefaultMonitorModeBufferInputCount == 0) return STATUS_SUCCESS;
    if (pInArgs->DefaultMonitorModeBufferInputCount < n) return STATUS_BUFFER_TOO_SMALL;
    for (UINT i = 0; i < n; i++) pInArgs->pDefaultMonitorModes[i] = CreateMonitorMode(dm.modes[i]);
    pOutArgs->PreferredMonitorModeIdx = 0;
    Log(L"MonitorGetDefaultModes -> %u modes (preferred %ux%u)", n, dm.modes[0].Width, dm.modes[0].Height);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorQueryModes(IDDCX_MONITOR MonitorObject, const IDARG_IN_QUERYTARGETMODES* pInArgs, IDARG_OUT_QUERYTARGETMODES* pOutArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    auto& dm = DisplayModes::Get();
    std::lock_guard<std::mutex> g(dm.lock);
    const UINT n = static_cast<UINT>(dm.modes.size());
    pOutArgs->TargetModeBufferOutputCount = n;
    if (pInArgs->TargetModeBufferInputCount >= n) {
        for (UINT i = 0; i < n; i++) pInArgs->pTargetModes[i] = CreateTargetMode(dm.modes[i]);
        Log(L"MonitorQueryTargetModes -> %u modes (preferred %ux%u)", n, dm.modes[0].Width, dm.modes[0].Height);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorAssignSwapChain(IDDCX_MONITOR MonitorObject, const IDARG_IN_SETSWAPCHAIN* pInArgs)
{
    auto* pWrapper = WdfObjectGet_IndirectMonitorContextWrapper(MonitorObject);
    pWrapper->pContext->AssignSwapChain(pInArgs->hSwapChain, pInArgs->RenderAdapterLuid, pInArgs->hNextSurfaceAvailable);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorUnassignSwapChain(IDDCX_MONITOR MonitorObject)
{
    auto* pWrapper = WdfObjectGet_IndirectMonitorContextWrapper(MonitorObject);
    pWrapper->pContext->UnassignSwapChain();
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorGetPhysicalSize(IDDCX_MONITOR MonitorObject, IDARG_OUT_MONITORGETPHYSICALSIZE* pOutArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    pOutArgs->PhysicalWidth = kPhysicalWidthMm;
    pOutArgs->PhysicalHeight = kPhysicalHeightMm;
    return STATUS_SUCCESS;
}

// ---- IddCx 1.10 (8-bit SDR only; no HDR, no gamma)

_Use_decl_annotations_
NTSTATUS FastRdpIddParseMonitorDescription2(const IDARG_IN_PARSEMONITORDESCRIPTION2* pInArgs, IDARG_OUT_PARSEMONITORDESCRIPTION* pOutArgs)
{
    // Not called with REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE; we never report an EDID anyway.
    Log(L"ParseMonitorDescription2 (descriptor size %u) -> 0 modes", pInArgs->MonitorDescription.DataSize);
    pOutArgs->MonitorModeBufferOutputCount = 0;
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorQueryModes2(IDDCX_MONITOR MonitorObject, const IDARG_IN_QUERYTARGETMODES2* pInArgs, IDARG_OUT_QUERYTARGETMODES* pOutArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    auto& dm = DisplayModes::Get();
    std::lock_guard<std::mutex> g(dm.lock);
    const UINT n = static_cast<UINT>(dm.modes.size());
    pOutArgs->TargetModeBufferOutputCount = n;
    if (pInArgs->TargetModeBufferInputCount >= n) {
        for (UINT i = 0; i < n; i++) pInArgs->pTargetModes[i] = CreateTargetMode2(dm.modes[i]);
        Log(L"MonitorQueryTargetModes2 -> %u modes (preferred %ux%u)", n, dm.modes[0].Width, dm.modes[0].Height);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddAdapterCommitModes2(IDDCX_ADAPTER AdapterObject, const IDARG_IN_COMMITMODES2* pInArgs)
{
    UNREFERENCED_PARAMETER(AdapterObject);
    Log(L"AdapterCommitModes2: %u path(s)", pInArgs->PathCount);
    for (UINT i = 0; i < pInArgs->PathCount; i++) {
        const auto& p = pInArgs->pPaths[i];
        const auto& s = p.TargetVideoSignalInfo;
        Log(L"  path %u flags=0x%X active=%d %ux%u @ %u/%u", i, p.Flags, (p.Flags & IDDCX_PATH_FLAGS_ACTIVE) ? 1 : 0,
            s.activeSize.cx, s.activeSize.cy, s.vSyncFreq.Numerator, s.vSyncFreq.Denominator);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddAdapterQueryTargetInfo(IDDCX_ADAPTER AdapterObject, IDARG_IN_QUERYTARGET_INFO* pInArgs, IDARG_OUT_QUERYTARGET_INFO* pOutArgs)
{
    UNREFERENCED_PARAMETER(AdapterObject);
    UNREFERENCED_PARAMETER(pInArgs);
    pOutArgs->TargetCaps = IDDCX_TARGET_CAPS_NONE;
    pOutArgs->DitheringSupport.Rgb = IDDCX_BITS_PER_COMPONENT_NONE;
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorSetDefaultHdrMetaData(IDDCX_MONITOR MonitorObject, const IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA* pInArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS FastRdpIddMonitorSetGammaRamp(IDDCX_MONITOR MonitorObject, const IDARG_IN_SET_GAMMARAMP* pInArgs)
{
    UNREFERENCED_PARAMETER(MonitorObject);
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

#pragma endregion
