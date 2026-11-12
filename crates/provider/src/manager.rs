//! IWRdsProtocolManager - one instance per TermService start (shared by all our listeners).

use std::sync::Mutex;

use windows::Win32::System::RemoteDesktop::*;
use windows_core::{PCWSTR, Ref, implement};

use crate::listener::Listener;
use crate::util::{from_pcwstr, guard, guid_str};
use crate::{config, logi, logw};

#[implement(IWRdsProtocolManager)]
pub struct Manager {
    settings: Mutex<Option<IWRdsProtocolSettings>>,
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

impl Manager {
    pub fn new() -> Self {
        logi!("Manager: COM object created (CLSID {})", guid_str(&crate::CLSID_FASTRDP_PROTOCOL_MANAGER));
        Manager { settings: Mutex::new(None) }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        logi!("Manager: COM object destroyed");
    }
}

fn session_id_str(p: *const WTS_SESSION_ID) -> String {
    if p.is_null() {
        return "<null>".into();
    }
    // SAFETY: TermService passes a valid WRDS_SESSION_ID.
    let s = unsafe { &*p };
    format!("{} guid={}", s.SessionId, guid_str(&s.SessionUniqueGuid))
}

impl IWRdsProtocolManager_Impl for Manager_Impl {
    fn Initialize(&self, piwrdssettings: Ref<IWRdsProtocolSettings>, pwrdssettings: *const WRDS_SETTINGS) -> windows_core::Result<()> {
        guard("Manager::Initialize", || {
            let (ty, lvl) = if pwrdssettings.is_null() {
                (-1, -1)
            } else {
                // SAFETY: valid pointer from TermService.
                let s = unsafe { &*pwrdssettings };
                (s.WRdsSettingType.0, s.WRdsSettingLevel.0)
            };
            logi!("Manager::Initialize settingsIface={} WRDS_SETTINGS(type={ty}, level={lvl})", if piwrdssettings.is_null() { "null" } else { "present" });
            *self.settings.lock().unwrap() = piwrdssettings.cloned();
            Ok(())
        })
    }

    fn CreateListener(&self, wszlistenername: &PCWSTR) -> windows_core::Result<IWRdsProtocolListener> {
        guard("Manager::CreateListener", || {
            // SAFETY: NUL-terminated string from TermService.
            let name = unsafe { from_pcwstr(wszlistenername.0) };
            let cfg = config::load(&name);
            logi!(
                "Manager::CreateListener name='{name}' iddHwId='{}' virtualChannels={} deviceInput={} fromRegistry={}",
                cfg.idd_hardware_id,
                cfg.virtual_channels,
                cfg.device_input,
                cfg.from_registry
            );
            if !cfg.from_registry {
                logw!("Manager::CreateListener: no WinStations\\{name} key with LoadableProtocol_Object - using defaults");
            }
            Ok(Listener::new(cfg).into())
        })
    }

    fn NotifyServiceStateChange(&self, p: *const WTS_SERVICE_STATE) -> windows_core::Result<()> {
        guard("Manager::NotifyServiceStateChange", || {
            if !p.is_null() {
                // SAFETY: valid pointer from TermService.
                let s = unsafe { &*p };
                logi!("Manager::NotifyServiceStateChange rcmServiceState={} drainState={}", s.RcmServiceState.0, s.RcmDrainState.0);
            }
            Ok(())
        })
    }

    fn NotifySessionOfServiceStart(&self, sessionid: *const WTS_SESSION_ID) -> windows_core::Result<()> {
        guard("Manager::NotifySessionOfServiceStart", || {
            logi!("Manager::NotifySessionOfServiceStart session={}", session_id_str(sessionid));
            Ok(())
        })
    }

    fn NotifySessionOfServiceStop(&self, sessionid: *const WTS_SESSION_ID) -> windows_core::Result<()> {
        guard("Manager::NotifySessionOfServiceStop", || {
            logi!("Manager::NotifySessionOfServiceStop session={}", session_id_str(sessionid));
            Ok(())
        })
    }

    fn NotifySessionStateChange(&self, sessionid: *const WTS_SESSION_ID, eventid: u32) -> windows_core::Result<()> {
        guard("Manager::NotifySessionStateChange", || {
            logi!("Manager::NotifySessionStateChange session={} eventId={eventid}", session_id_str(sessionid));
            Ok(())
        })
    }

    fn NotifySettingsChange(&self, p: *const WRDS_SETTINGS) -> windows_core::Result<()> {
        guard("Manager::NotifySettingsChange", || {
            logi!("Manager::NotifySettingsChange settings={}", if p.is_null() { "null" } else { "present" });
            Ok(())
        })
    }

    fn Uninitialize(&self) -> windows_core::Result<()> {
        guard("Manager::Uninitialize", || {
            *self.settings.lock().unwrap() = None;
            Ok(())
        })
    }
}
