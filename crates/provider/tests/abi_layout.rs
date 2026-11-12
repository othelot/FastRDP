//! Struct layouts of the `windows` crate bindings vs. the Windows SDK C headers (wtsdefs.h).
//! Expected values: `sizeof`/`offsetof` from MSVC against SDK 10.0.28000 (re-check after an SDK or
//! `windows` crate upgrade).
use std::mem::{offset_of, size_of};

use windows::Win32::System::RemoteDesktop::*;

#[test]
fn rds_structs_match_sdk_headers() {
    assert_eq!(size_of::<WTS_CLIENT_DATA>(), 3712);
    assert_eq!(offset_of!(WTS_CLIENT_DATA, ProtocolName), 2588);
    assert_eq!(offset_of!(WTS_CLIENT_DATA, HRes), 2612);
    assert_eq!(offset_of!(WTS_CLIENT_DATA, ClientName), 2960);
    assert_eq!(offset_of!(WTS_CLIENT_DATA, ClientAddressFamily), 3008);
    assert_eq!(size_of::<WTS_USER_CREDENTIAL>(), 1536);
    assert_eq!(offset_of!(WTS_USER_CREDENTIAL, Domain), 1024);
    assert_eq!(size_of::<WRDS_CONNECTION_SETTINGS>(), 3768);
    assert_eq!(offset_of!(WRDS_CONNECTION_SETTINGS_1, WRdsListenerSettings), 3704);
    assert_eq!(size_of::<WRDS_SETTINGS>(), 132);
    assert_eq!(size_of::<WTS_SESSION_ID>(), 20);
    assert_eq!(size_of::<WTS_LICENSE_CAPABILITIES>(), 64);
    assert_eq!(size_of::<WTS_PROTOCOL_STATUS>(), 1824);
    assert_eq!(size_of::<WTS_SERVICE_STATE>(), 8);
}
