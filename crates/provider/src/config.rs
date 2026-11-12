//! Per-listener configuration, read from
//! `HKLM\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\<ListenerName>`.
//!
//! | value                    | type  | default             | meaning |
//! |--------------------------|-------|---------------------|---------|
//! | `FastRdpIddHardwareId`   | SZ    | `FastRDP_RemoteIDD` | returned by `IWRdsWddmIddProps::GetHardwareId` |
//! | `FastRdpVirtualChannels` | DWORD | 1                   | virtual channels (clipboard, ...) and audio playback |
//! | `FastRdpDeviceInput`     | DWORD | 1                   | 1 = session keyboard/mouse devices like RDP (terminpt.sys); 0 = none (the server uses the SendInput agent) |
//!
//! The provider talks to fastrdp-server over the local named pipe `\\.\pipe\FastRDP.Provider.<Listener>`
//! only; TermService opens no network port.

use crate::util::{reg_dword, reg_string};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;

pub const WINSTATIONS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations";

#[derive(Clone, Debug)]
pub struct ListenerConfig {
    pub listener_name: String,
    pub idd_hardware_id: String,
    pub from_registry: bool,
    pub device_input: bool,
    pub virtual_channels: bool,
}

impl ListenerConfig {
    /// `\\.\pipe\FastRDP.Provider.<Listener>`
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\FastRDP.Provider.{}", self.listener_name)
    }

    pub fn defaults(name: &str) -> Self {
        ListenerConfig {
            listener_name: name.to_string(),
            idd_hardware_id: "FastRDP_RemoteIDD".into(),
            from_registry: false,
            device_input: true,
            virtual_channels: true,
        }
    }
}

pub fn load(listener_name: &str) -> ListenerConfig {
    let mut c = ListenerConfig::defaults(listener_name);
    let key = format!(r"{WINSTATIONS_KEY}\{listener_name}");
    if !listener_name.is_empty() && reg_string(HKEY_LOCAL_MACHINE, &key, "LoadableProtocol_Object").is_some() {
        c.from_registry = true;
    }
    if let Some(v) = reg_dword(HKEY_LOCAL_MACHINE, &key, "FastRdpVirtualChannels") {
        c.virtual_channels = v != 0;
    }
    if let Some(v) = reg_dword(HKEY_LOCAL_MACHINE, &key, "FastRdpDeviceInput") {
        c.device_input = v != 0;
    }
    if let Some(v) = reg_string(HKEY_LOCAL_MACHINE, &key, "FastRdpIddHardwareId").filter(|s| !s.is_empty()) {
        c.idd_hardware_id = v;
    }
    c
}
