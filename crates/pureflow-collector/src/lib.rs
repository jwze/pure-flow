// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Native collectors. No browser shell is ever elevated for sampling.
use pureflow_core::RawSample;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CollectorError {
    #[error("collector unavailable: {0}")]
    Unavailable(String),
}

pub mod clock;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_schema;

pub enum NativeCollector {
    #[cfg(target_os = "macos")]
    Mac(macos::MacCollector),
    #[cfg(windows)]
    Windows(windows::WindowsCollector),
}
impl NativeCollector {
    pub fn start() -> Result<Self, CollectorError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self::Mac(macos::MacCollector::start()?))
        }
        #[cfg(windows)]
        {
            Ok(Self::Windows(windows::WindowsCollector::start()?))
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            Err(CollectorError::Unavailable(
                "only macOS and Windows are supported".into(),
            ))
        }
    }
    /// Windows helpers must report ETW/session/journal cleanup before replying
    /// to Pause or Stop. Other platforms retain their existing Drop lifecycle.
    #[cfg(windows)]
    pub fn shutdown(&mut self) -> Result<(), CollectorError> {
        match self {
            Self::Windows(collector) => collector.shutdown(),
        }
    }
    pub fn sample(&mut self) -> Result<RawSample, CollectorError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Mac(collector) => collector.sample(),
            #[cfg(windows)]
            Self::Windows(collector) => collector.sample(),
            #[cfg(not(any(target_os = "macos", windows)))]
            _ => Err(CollectorError::Unavailable("unsupported platform".into())),
        }
    }
}
