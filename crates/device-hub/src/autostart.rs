//! 常駐まわり (G HUB の置き換えとして常時起動させる)。
//!
//! - Windows ログオン時の自動起動: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!   の `DeviceHub` 値に `"<exe>" --background` を書く (管理者権限不要)。
//! - `--background`: ウィンドウを最小化した状態で起動する。
//! - 閉じても常駐: × で終了せずに最小化する (プロセス監視・電池監視を続ける)。
//! - 多重起動防止: 2 個目は既存ウィンドウを前面に出して終了する
//!   (同じマウスの HID を 2 プロセスで取り合わない)。

use std::sync::atomic::{AtomicBool, Ordering};

/// 起動引数: 最小化して起動する。
pub const BACKGROUND_ARG: &str = "--background";

static KEEP_RUNNING: AtomicBool = AtomicBool::new(true);

/// × ボタンで終了せず最小化するか。
pub fn keep_running() -> bool {
    KEEP_RUNNING.load(Ordering::Relaxed)
}

pub fn set_keep_running(on: bool) {
    KEEP_RUNNING.store(on, Ordering::Relaxed);
}

/// `--background` で起動されたか。
pub fn started_in_background() -> bool {
    std::env::args().any(|a| a == BACKGROUND_ARG)
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "DeviceHub";

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn command_line() -> Result<String, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Ok(format!("\"{}\" {}", exe.display(), super::BACKGROUND_ARG))
    }

    fn read_value() -> Option<String> {
        let key = w(RUN_KEY);
        let val = w(VALUE);
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                val.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = (len as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
    }

    pub fn is_enabled() -> bool {
        // 別の場所の exe を指していたら (移動・更新後) 無効扱い → ON で書き直せる。
        match (read_value(), command_line()) {
            (Some(v), Ok(cmd)) => v.eq_ignore_ascii_case(&cmd),
            _ => false,
        }
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        let key = w(RUN_KEY);
        let val = w(VALUE);
        let rc = if on {
            let data = w(&command_line()?);
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    val.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            }
        } else {
            let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr()) };
            // 元から無いのは成功扱い (ERROR_FILE_NOT_FOUND = 2)。
            if rc == 2 {
                ERROR_SUCCESS
            } else {
                rc
            }
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("レジストリ書き込みに失敗しました (code {rc})"))
        }
    }

    /// 多重起動なら既存ウィンドウを前面に出して true。
    pub fn already_running() -> bool {
        let name = w("Local\\DeviceHub.SingleInstance");
        // ハンドルはプロセス終了まで持ち続ける (閉じると別インスタンスが取れる)。
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if h.is_null() {
            return false;
        }
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            return false;
        }
        let title = w("Device HUB");
        let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if !hwnd.is_null() {
            unsafe {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
        }
        true
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }
    pub fn set_enabled(_on: bool) -> Result<(), String> {
        Err("自動起動は Windows のみ対応です".into())
    }
    pub fn already_running() -> bool {
        false
    }
}

/// Windows ログオン時に自動起動する設定になっているか。
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// 自動起動の ON/OFF。
pub fn set_enabled(on: bool) -> Result<(), String> {
    imp::set_enabled(on)
}

/// すでに起動しているか (していれば既存ウィンドウを前面に出す)。
pub fn already_running() -> bool {
    imp::already_running()
}
