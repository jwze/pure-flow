// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Bounded, memory-only popup preferences. Never persisted into the WebView cache.
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::fmt;
use std::sync::Mutex;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tab {
    #[default]
    Live,
    History,
    Settings,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Range {
    #[default]
    Today,
    Yesterday,
    Week,
    Custom,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    #[default]
    Total,
    Received,
    Sent,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanelState {
    pub tab: Tab,
    pub range: Range,
    #[serde(deserialize_with = "date")]
    pub from: String,
    #[serde(deserialize_with = "date")]
    pub to: String,
    #[serde(deserialize_with = "network")]
    pub network: Option<String>,
    #[serde(deserialize_with = "search")]
    pub live_search: String,
    pub live_sort: Sort,
    #[serde(deserialize_with = "search")]
    pub history_search: String,
    pub history_sort: Sort,
}
fn valid_date(date: &str) -> bool {
    date.is_empty()
        || (date.len() == 10
            && date.bytes().enumerate().all(|(i, b)| {
                if i == 4 || i == 7 {
                    b == b'-'
                } else {
                    b.is_ascii_digit()
                }
            }))
}
struct Text {
    max: usize,
    is_date: bool,
}
impl<'de> Visitor<'de> for Text {
    type Value = String;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bounded panel text (max {} UTF-8 bytes)", self.max)
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        if value.len() > self.max || (self.is_date && !valid_date(value)) {
            return Err(E::custom("弹层文本超出允许范围"));
        }
        Ok(value.to_owned())
    }
}
fn date<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    d.deserialize_str(Text {
        max: 10,
        is_date: true,
    })
}
fn search<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    d.deserialize_str(Text {
        max: 1024,
        is_date: false,
    })
}
fn network<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct Network;
    impl<'de> Visitor<'de> for Network {
        type Value = Option<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("SSID or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_str(Text {
                max: 256,
                is_date: false,
            })
            .map(Some)
        }
    }
    d.deserialize_option(Network)
}
#[derive(Default)]
pub struct Memory(Mutex<PanelState>);
impl Memory {
    pub fn get(&self) -> Result<PanelState, String> {
        self.0
            .lock()
            .map(|value| value.clone())
            .map_err(|_| "弹层状态锁不可用".into())
    }
    pub fn save(&self, state: PanelState) -> Result<(), String> {
        if !valid_date(&state.from)
            || !valid_date(&state.to)
            || state
                .network
                .as_ref()
                .is_some_and(|value| value.len() > 256)
            || state.live_search.len() > 1024
            || state.history_search.len() > 1024
        {
            return Err("弹层状态超出允许范围".into());
        }
        *self.0.lock().map_err(|_| "弹层状态锁不可用")? = state;
        Ok(())
    }
}
