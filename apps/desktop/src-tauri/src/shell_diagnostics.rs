// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Low-volume popup event journal. No traffic, process names, or window titles.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: u64 = 256 * 1024;

pub struct Journal {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl Journal {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn record(&self, event: &str) {
        let Some(path) = &self.path else { return };
        let Ok(_guard) = self.lock.lock() else { return };
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            if fs::metadata(path).is_ok_and(|m| m.len() >= MAX_BYTES) {
                let previous = path.with_extension("previous.log");
                match fs::remove_file(&previous) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e),
                }
                fs::rename(path, previous)?;
            }
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            // Bound each entry too, including errors from external APIs.
            let line: String = event
                .chars()
                .take(1024)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            writeln!(
                OpenOptions::new().create(true).append(true).open(path)?,
                "{timestamp} pid={} {line}",
                std::process::id()
            )
        })();
        if let Err(error) = result {
            eprintln!("popup journal unavailable: {error}");
        }
    }
}

#[cfg(windows)]
pub struct Foreground {
    pub panel: bool,
    pub shell: bool,
    pub class: String,
}

#[cfg(windows)]
pub fn foreground(window: &tauri::WebviewWindow) -> Foreground {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetClassNameW, GetForegroundWindow, GA_ROOT,
    };
    // SAFETY: read-only queries of live handles; GetClassNameW receives a valid bounded buffer.
    unsafe {
        let foreground = GetForegroundWindow();
        let panel = window.hwnd().is_ok_and(|hwnd| {
            !foreground.is_null() && GetAncestor(foreground, GA_ROOT) == hwnd.0.cast()
        });
        let mut buffer = [0u16; 128];
        let len =
            GetClassNameW(foreground, buffer.as_mut_ptr(), buffer.len() as i32).max(0) as usize;
        let class = String::from_utf16_lossy(&buffer[..len]);
        let shell = matches!(
            class.as_str(),
            "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "NotifyIconOverflowWindow"
                | "TopLevelWindowForOverflowXamlIsland"
        );
        Foreground {
            panel,
            shell,
            class,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_rotates_and_sanitizes_entries() {
        let path = std::env::temp_dir()
            .join(format!("pureflow-popup-log-test-{}", std::process::id()))
            .join("popup.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        Journal::new(Some(path.clone())).record("show\nblur");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("show blur"));
        assert_eq!(content.lines().count(), 1);
        assert_eq!(
            fs::metadata(path.with_extension("previous.log"))
                .unwrap()
                .len(),
            MAX_BYTES
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
