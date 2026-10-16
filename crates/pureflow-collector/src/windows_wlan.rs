// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! WLAN current-connection SSID, never a profile name. GUID joins prevent adapter confusion.
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{addr_of, null_mut},
};
use windows::{
    core::GUID,
    Win32::{
        Foundation::{ERROR_ACCESS_DENIED, HANDLE},
        NetworkManagement::WiFi::*,
    },
};

struct Client(HANDLE);
impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.0, None);
        }
    }
}
struct Memory(*mut c_void);
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            WlanFreeMemory(self.0);
        }
    }
}
fn error(api: &str, code: u32) -> String {
    if code == ERROR_ACCESS_DENIED.0 {
        format!("{api}: access denied ({code}); Windows location/privacy permission may be required for WLAN SSID; all-network statistics only")
    } else {
        format!("{api} failed ({code}); WLAN service/connection may be unavailable; all-network statistics only")
    }
}
/// RawSample has one global network label: only one physical interface, joined to one WLAN
/// connection, is safe. Even identical SSIDs on two interfaces do not prove one byte context.
pub(super) fn network(physical: &[(GUID, u32)], warnings: &mut Vec<String>) -> Option<String> {
    if !physical.iter().any(|(_, ty)| *ty == 71) {
        return None;
    }
    if physical.len() != 1 {
        warnings.push("multiple active physical interfaces; a single SSID cannot own their aggregate bytes; all-network statistics only".into());
        return None;
    }
    let mut handle = HANDLE::default();
    let mut version = 0;
    let rc = unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) };
    if rc != 0 {
        warnings.push(error("WlanOpenHandle", rc));
        return None;
    }
    let client = Client(handle);
    let mut list = null_mut();
    let rc = unsafe { WlanEnumInterfaces(client.0, None, &mut list) };
    if rc != 0 {
        warnings.push(error("WlanEnumInterfaces", rc));
        return None;
    }
    if list.is_null() {
        warnings.push("WlanEnumInterfaces returned null".into());
        return None;
    }
    let _list_memory = Memory(list.cast());
    let count = unsafe { (*list).dwNumberOfItems as usize };
    if count > 1024 {
        warnings.push("WLAN interface list exceeds safety limit".into());
        return None;
    }
    let interfaces = unsafe {
        std::slice::from_raw_parts(
            addr_of!((*list).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
            count,
        )
    };
    let connected: Vec<_> = interfaces
        .iter()
        .filter(|v| v.isState == wlan_interface_state_connected)
        .collect();
    if connected.len() != 1 || connected[0].InterfaceGuid != physical[0].0 {
        warnings.push("WLAN connected interface cannot be uniquely joined to the physical counter GUID; all-network statistics only".into());
        return None;
    }
    let mut data = null_mut();
    let mut size = 0;
    let rc = unsafe {
        WlanQueryInterface(
            client.0,
            &connected[0].InterfaceGuid,
            wlan_intf_opcode_current_connection,
            None,
            &mut size,
            &mut data,
            None,
        )
    };
    if rc != 0 {
        warnings.push(error("WlanQueryInterface(current_connection)", rc));
        return None;
    }
    if data.is_null() {
        warnings.push("WLAN current connection returned null".into());
        return None;
    }
    let _data_memory = Memory(data);
    if (size as usize) < size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
        warnings.push("WLAN current connection buffer is truncated".into());
        return None;
    }
    let attributes = unsafe { &*data.cast::<WLAN_CONNECTION_ATTRIBUTES>() };
    if attributes.isState != wlan_interface_state_connected {
        warnings.push("WLAN disconnected during SSID query; all-network statistics only".into());
        return None;
    }
    let ssid = &attributes.wlanAssociationAttributes.dot11Ssid;
    let Some(name) = ssid_name(&ssid.ucSSID, ssid.uSSIDLength as usize) else {
        warnings.push("WLAN SSID is empty/invalid/non-UTF-8; cannot represent it without aliasing in RawSample; all-network statistics only".into());
        return None;
    };
    Some(name)
}
fn ssid_name(bytes: &[u8; 32], len: usize) -> Option<String> {
    if len == 0 || len > bytes.len() {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..len]).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(text.to_owned())
}
