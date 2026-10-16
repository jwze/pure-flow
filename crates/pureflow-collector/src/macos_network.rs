// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Current CoreWLAN SSID observation, never remembered networks or scan caches.
//!
//! Reading `CWInterface::ssid` does not request Location authorization. On modern
//! macOS the signed GUI app must declare its Location usage in Info.plist, and
//! offer an explicit user action to authorize SSID access via CoreLocation on
//! the main thread. Sampling must not request permission, start location updates,
//! or bypass TCC. A nil SSID is ambiguous (privacy denial, disconnected interface,
//! encoding failure, or API error); it is never replaced with a previous value.

#[cfg(target_os = "macos")]
use objc2::rc::autoreleasepool;
#[cfg(target_os = "macos")]
use objc2_core_wlan::CWWiFiClient;
use pureflow_core::InterfaceCounter;

pub(super) struct NetworkReading {
    pub network: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct WifiInterface {
    name: Option<String>,
    ssid: Option<String>,
}

#[cfg(target_os = "macos")]
pub(super) fn read(interfaces: &[InterfaceCounter]) -> NetworkReading {
    // Keep Objective-C objects inside this thread's pool. Use the framework's
    // long-lived singleton, but query interfaces and current SSID on every
    // sample: no cached SSID, no preferred-network list, no networksetup parsing.
    autoreleasepool(|_| {
        // SAFETY: these typed CoreWLAN getters take no raw pointers, invoke no
        // callbacks, and have no main-thread requirement. Retained objects stay
        // alive through conversion to owned Rust strings within this pool. We
        // neither mutate interfaces nor install a delegate on the shared client.
        let observed = unsafe {
            let client = CWWiFiClient::sharedWiFiClient();
            client.interfaces().map(|wifi| {
                (0..wifi.count())
                    .map(|index| {
                        let interface = wifi.objectAtIndex(index);
                        WifiInterface {
                            name: interface.interfaceName().map(|name| name.to_string()),
                            ssid: interface.ssid().map(|ssid| ssid.to_string()),
                        }
                    })
                    .collect::<Vec<_>>()
            })
        };
        attribute(interfaces, observed.as_deref())
    })
}

fn unknown(message: &str) -> NetworkReading {
    NetworkReading {
        network: None,
        warnings: vec![format!(
            "macOS SSID unavailable: {message}; all-network statistics only"
        )],
    }
}

fn attribute(interfaces: &[InterfaceCounter], wifi: Option<&[WifiInterface]>) -> NetworkReading {
    let Some(wifi) = wifi else {
        return unknown("CoreWLAN could not enumerate Wi-Fi interfaces");
    };
    if wifi.is_empty() {
        return unknown("CoreWLAN reports no Wi-Fi interfaces");
    }
    // Do not pick the default/first interface, or merge matching SSIDs. A nil
    // SSID on a second adapter could be privacy-redacted, not disconnected.
    if wifi.len() != 1 {
        return unknown("multiple Wi-Fi interfaces make network attribution ambiguous");
    }
    // RawSample's network labels the sum of *all* sampled physical counters.
    // Even an apparently idle/offline Ethernet adapter prevents proving sole
    // ownership. Do not infer ownership from lifetime counters or route order.
    if interfaces.len() != 1 {
        return unknown(
            "multiple or missing physical interface counters make network attribution ambiguous",
        );
    }
    let wifi = &wifi[0];
    let Some(name) = wifi.name.as_deref().filter(|name| !name.is_empty()) else {
        return unknown("CoreWLAN did not identify the Wi-Fi interface");
    };
    if interfaces[0].name != name {
        return unknown("the sampled physical interface is not the CoreWLAN Wi-Fi interface");
    }
    let Some(ssid) = wifi.ssid.as_deref().filter(|ssid| !ssid.is_empty()) else {
        // CoreWLAN exposes no authorization status/error for this getter; do
        // not assert a particular denial or invent a disconnected network.
        return unknown("CoreWLAN returned no current SSID; macOS privacy may deny access (Location Services must be enabled and the app authorized), or Wi-Fi is disconnected / the SSID cannot be decoded / the API failed. Grant access only via an explicit app action or System Settings");
    };
    NetworkReading {
        // Preserve the actual SSID, including spaces/non-ASCII; never trim,
        // normalize, substitute a placeholder, or fall back to older data.
        network: Some(ssid.to_owned()),
        warnings: vec!["macOS SSID is a current CoreWLAN observation; CoreWLAN and netstat snapshots are not atomic, so a Wi-Fi switch between observations cannot be fully disambiguated".into()],
    }
}
