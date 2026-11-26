//! "Start with Windows" via the per-user Run key (no admin rights needed).

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;
use windows::core::*;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("sp-lyrics-rs");

/// The command line Windows runs at sign-in: this exe, quoted.
fn command() -> Option<String> {
    Some(format!("\"{}\"", std::env::current_exe().ok()?.display()))
}

/// True if the Run entry exists and points at this exe (a moved exe counts as off).
pub fn is_enabled() -> bool {
    let Some(cmd) = command() else { return false };
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let res = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    if res != ERROR_SUCCESS {
        return false;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).eq_ignore_ascii_case(&cmd)
}

pub fn set(enabled: bool) -> bool {
    unsafe {
        if !enabled {
            let res = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
            return res == ERROR_SUCCESS || !is_enabled();
        }
        let Some(cmd) = command() else { return false };
        let data: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
        let res = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE,
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        );
        res == ERROR_SUCCESS
    }
}
