// FastRdpIdd - Remote Indirect Display Driver for FastRDP sessions (UMDF 2; built against IddCx 1.10,
// runs on IddCx >= 1.4. With 1.10 (Windows 11 24H2+) it uses the *2 mode DDIs and
// IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE like the inbox RdpIdd, which is what
// allows arbitrary client resolutions mid-session.)
//
// Based on Microsoft's Windows-driver-samples video/IndirectDisplay (MIT) with remote-IDD changes;
// frame-loop structure informed by VirtualDrivers/Virtual-Display-Driver (MIT).
//
// The device is created by the Windows remote desktop stack as
//   SWD\RemoteDisplayEnum\FastRDP_RemoteIDD&SessionId_NNNN
// when FastRdpProvider returns "FastRDP_RemoteIDD" from IWRdsWddmIddProps::GetHardwareId.
#pragma once

#define NOMINMAX
#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>

#include <dxgi1_5.h>
#include <d3d11_2.h>
#include <avrt.h>
#include <wrl.h>

#include <memory>
#include <vector>
#include <algorithm>
#include <atomic>
#include <string>
#include <mutex>

namespace Microsoft::WRL::Wrappers {
typedef HandleT<HandleTraits::HANDLENullTraits> Thread;
}

namespace FastRdp {

// Logging: %ProgramData%\FastRDP\logs\idd.log (installer grants LOCAL SERVICE write) + OutputDebugString.
void Log(_Printf_format_string_ const wchar_t* fmt, ...);

// True when IddCx 1.10 DDIs are available (decided once in DeviceAdd).
extern bool g_ModernModes;

struct ModeEntry {
    DWORD Width;
    DWORD Height;
    DWORD RefreshHz;
};

struct Direct3DDevice {
    explicit Direct3DDevice(LUID AdapterLuid) : AdapterLuid(AdapterLuid) {}
    HRESULT Init();

    LUID AdapterLuid;
    std::wstring AdapterDescription;
    Microsoft::WRL::ComPtr<IDXGIFactory5> DxgiFactory;
    Microsoft::WRL::ComPtr<IDXGIAdapter1> Adapter;
    Microsoft::WRL::ComPtr<ID3D11Device> Device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> DeviceContext;
};

// Consumes frames from an IddCx swap-chain on a dedicated MMCSS thread.
class SwapChainProcessor {
public:
    SwapChainProcessor(IDDCX_MONITOR Monitor, IDDCX_SWAPCHAIN hSwapChain, std::shared_ptr<Direct3DDevice> Device, HANDLE NewFrameEvent, ULONG SessionId);
    ~SwapChainProcessor();

private:
    static DWORD CALLBACK RunThread(LPVOID Argument);
    void Run();
    // Returns true when the swap-chain was lost (driver must delete it so IddCx re-assigns a new one).
    bool RunCore();
    // Hardware cursor: the OS then stops drawing the pointer into the frames and reports shape and
    // position separately (published to shared memory, rendered by the client).
    void SetupCursor();
    static DWORD CALLBACK CursorThread(LPVOID Argument);
    void CursorLoop();

    IDDCX_MONITOR m_Monitor;
    IDDCX_SWAPCHAIN m_hSwapChain;
    std::shared_ptr<Direct3DDevice> m_Device;
    HANDLE m_hAvailableBufferEvent;
    ULONG m_SessionId;
    Microsoft::WRL::Wrappers::Thread m_hThread;
    Microsoft::WRL::Wrappers::Event m_hTerminateEvent;
    Microsoft::WRL::Wrappers::Event m_hCursorEvent;
    Microsoft::WRL::Wrappers::Event m_hCursorStop;
    Microsoft::WRL::Wrappers::Thread m_hCursorThread;
};

class IndirectDeviceContext {
public:
    explicit IndirectDeviceContext(_In_ WDFDEVICE WdfDevice);
    ~IndirectDeviceContext();

    void InitAdapter();
    void FinishInit();
    ULONG SessionId() const { return m_SessionId; }

private:
    // Sets the session's display configuration (IddCxAdapterDisplayConfigUpdate) to `mode`.
    NTSTATUS ApplyDisplayConfig(const ModeEntry& mode, UINT scalePercent);
    // Client-driven mode changes: waits for the server's control event, then
    // IddCxMonitorUpdateModes + IddCxAdapterDisplayConfigUpdate (remote IDD "scenario 4").
    static DWORD CALLBACK ControlThread(LPVOID Argument);
    void ControlLoop();
    // Before monitor arrival: puts the client's requested mode (if the server wrote one) in front of
    // the target mode list the OS queries during arrival.
    void TakeInitialRequest();
    void ProcessControl(bool initial);

    WDFDEVICE m_WdfDevice;
    IDDCX_ADAPTER m_Adapter{};
    IDDCX_MONITOR m_Monitor{};
    ULONG m_SessionId = 0xFFFFFFFF;
    std::wstring m_InstanceId;
    // request taken by TakeInitialRequest, answered by the initial ProcessControl (0 = none)
    uint32_t m_InitialRequestId = 0;
    Microsoft::WRL::Wrappers::Event m_hStopControl;
    Microsoft::WRL::Wrappers::Thread m_hControlThread;
};

// Process-wide display mode state (one remote IDD device per WUDFHost: ProcessSharingDisabled).
struct DisplayModes {
    std::mutex lock;
    std::vector<ModeEntry> modes;   // reported target/monitor modes
    ModeEntry current{ 1920, 1080, 60 };
    UINT scale = 100;
    static DisplayModes& Get();
    // Adds `m` (front = preferred) and returns the list snapshot.
    std::vector<ModeEntry> Prefer(const ModeEntry& m);
};

class IndirectMonitorContext {
public:
    IndirectMonitorContext(_In_ IDDCX_MONITOR Monitor, ULONG SessionId);
    ~IndirectMonitorContext();

    void AssignSwapChain(IDDCX_SWAPCHAIN SwapChain, LUID RenderAdapter, HANDLE NewFrameEvent);
    void UnassignSwapChain();

private:
    IDDCX_MONITOR m_Monitor;
    ULONG m_SessionId;
    std::unique_ptr<SwapChainProcessor> m_ProcessingThread;
};

} // namespace FastRdp
