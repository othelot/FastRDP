//! FastRDP custom Remote Desktop Services protocol provider.
//!
//! In-proc COM server (`ThreadingModel=Both`) that TermService's Remote Connection Manager
//! instantiates via `CoCreateInstance(CLSID_FastRdpProtocolManager, CLSCTX_INPROC_SERVER,
//! IID_IWRdsProtocolManager)` when a `WinStations\<Listener>` key names our CLSID in
//! `LoadableProtocol_Object`.
//!
//! fastrdp-server hands each authenticated client connection to the provider over a local named
//! pipe; the provider forwards it to TermService (`OnConnected` / `OnReady`) so Windows creates a
//! remote session, and answers the connection sequence (credentials, IDD, input, channels).
#![allow(non_snake_case)]
// COM method signatures are dictated by the `windows` crate traits (raw pointers from TermService);
// every dereference is null-checked and documented with a SAFETY comment.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod channels;
pub mod config;
pub mod connection;
pub mod listener;
pub mod log;
pub mod manager;
pub mod terminput;
pub mod transport;
pub mod util;

use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::RemoteDesktop::IWRdsProtocolManager;
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, implement};
use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW};

/// {80999561-74B1-4880-9408-646E4A5B5187} - must match scripts/*.ps1
pub const CLSID_FASTRDP_PROTOCOL_MANAGER: GUID = GUID::from_u128(0x80999561_74b1_4880_9408_646e4a5b5187);
pub const PROTOCOL_NAME: &str = "FastRDP"; // WRDS_CLIENT_DATA::ProtocolName (max 8 incl. NUL)

const S_OK: HRESULT = HRESULT(0);
const S_FALSE: HRESULT = HRESULT(1);
const CLASS_E_CLASSNOTAVAILABLE: HRESULT = HRESULT(0x80040111u32 as i32);
const CLASS_E_NOAGGREGATION: HRESULT = HRESULT(0x80040110u32 as i32);

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static INIT: Once = Once::new();

pub fn module_handle() -> *mut c_void {
    MODULE.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst, Ordering::Relaxed);
    }
    BOOL(1)
}

/// One-time init: pin the module (worker threads run our code, so we must never be unloaded
/// while TermService lives), install a panic hook that logs, and log the environment.
pub fn one_time_init(reason: &str) {
    INIT.call_once(|| {
        let mut h = std::ptr::null_mut();
        // SAFETY: address inside this module; PIN keeps it loaded for the process lifetime.
        unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN, one_time_init as *const () as *const u16, &mut h) };
        std::panic::set_hook(Box::new(|info| {
            loge!("panic: {info}");
        }));
        util::log_environment(reason);
    });
}

#[implement(IClassFactory)]
struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        util::guard("ClassFactory::CreateInstance", || {
            if ppv.is_null() || riid.is_null() {
                return Err(util::E_POINTER.into());
            }
            // SAFETY: checked non-null above.
            unsafe { *ppv = std::ptr::null_mut() };
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            // SAFETY: checked non-null.
            let iid = unsafe { *riid };
            logi!("ClassFactory::CreateInstance riid={}", util::guid_str(&iid));
            let mgr: IWRdsProtocolManager = manager::Manager::new().into();
            // SAFETY: standard QueryInterface out-parameter contract.
            unsafe { mgr.query(&iid, ppv).ok() }
        })
    }

    fn LockServer(&self, lock: BOOL) -> windows_core::Result<()> {
        logi!("ClassFactory::LockServer({})", lock.0);
        Ok(())
    }
}

/// # Safety
/// COM export; pointers come from the COM runtime.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let r = std::panic::catch_unwind(|| {
        if ppv.is_null() || rclsid.is_null() || riid.is_null() {
            return util::E_POINTER;
        }
        // SAFETY: checked non-null.
        unsafe { *ppv = std::ptr::null_mut() };
        one_time_init("DllGetClassObject");
        // SAFETY: checked non-null.
        let (clsid, iid) = unsafe { (*rclsid, *riid) };
        if clsid != CLSID_FASTRDP_PROTOCOL_MANAGER {
            logw!("DllGetClassObject: unknown CLSID {}", util::guid_str(&clsid));
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let f: IClassFactory = ClassFactory.into();
        // SAFETY: standard QueryInterface out-parameter contract.
        let hr = unsafe { f.query(&iid, ppv) };
        logi!("DllGetClassObject(clsid={}, riid={}) -> hr=0x{:08X}", util::guid_str(&clsid), util::guid_str(&iid), hr.0 as u32);
        hr
    });
    r.unwrap_or(util::E_UNEXPECTED)
}

/// The module is pinned once used, so it is never unloaded while worker threads may run.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if INIT.is_completed() { S_FALSE } else { S_OK }
}
