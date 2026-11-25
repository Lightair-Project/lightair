//! A VST3's own editor on Windows, in a window of its own: a plain Win32 window whose client
//! area hosts the plugin's view (the plugin makes its child window in it). GPUI's message loop
//! runs on the same thread, so the window needs no loop of its own. Main thread only.
//!
//! Closing the window hides it; the window is destroyed when the editor drops, after the plugin's
//! view has been taken off it.

use std::sync::Arc;

use lightair_obs::plugins::vst3::{Vst3Plugin, Vst3View};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    IsIconic, IsWindow, IsWindowVisible, RegisterClassW, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow, WM_CLOSE, WNDCLASSW, WS_CAPTION,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};

const STYLE: u32 = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        // hide, don't destroy: the plugin's view is still in it (see Editor's Drop)
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// The window class, registered once.
fn class() -> *const u16 {
    static NAME: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let name = wide("LightairPluginEditor");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
            lpszClassName: name.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        unsafe { RegisterClassW(&wc) };
        name
    })
    .as_ptr()
}

/// The outer size for a client area of `w`×`h`.
fn outer(w: i32, h: i32) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
    unsafe { AdjustWindowRectEx(&mut r, STYLE, 0, 0) };
    (r.right - r.left, r.bottom - r.top)
}

/// An open editor window.
pub struct Editor {
    /// Keeps the plugin and its attached view alive while it shows; taken off before the window
    /// goes.
    content: Option<(Vst3View, Arc<Vst3Plugin>)>,
    window: HWND,
}

impl Editor {
    /// A VST3's own editor in a window titled `title`, shown.
    pub fn open_vst3(plugin: Arc<Vst3Plugin>, title: &str) -> Option<Editor> {
        let (view, (w, h)) = plugin.create_view()?;
        let (ow, oh) = outer(w.max(200), h.max(100));
        let title = wide(title);
        let window = unsafe {
            CreateWindowExW(
                0,
                class(),
                title.as_ptr(),
                STYLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                ow,
                oh,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return None;
        }
        // the plugin asks for new sizes; the window outlives the view (Drop)
        let win = window as usize;
        let resize = move |w: i32, h: i32| {
            let (ow, oh) = outer(w, h);
            unsafe {
                SetWindowPos(win as HWND, std::ptr::null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE)
            };
        };
        let Some(attached) = (unsafe { Vst3View::attach(view, window, resize) }) else {
            unsafe { DestroyWindow(window) };
            return None;
        };
        // a background copy (tests, screenshots) shows it without taking the keyboard
        let show = if std::env::var_os("LIGHTAIR_OPEN").is_some() { SW_SHOWNOACTIVATE } else { SW_SHOW };
        unsafe { ShowWindow(window, show) };
        Some(Editor { content: Some((attached, plugin)), window })
    }

    /// Still open (showing or minimised): the user hasn't closed it.
    pub fn is_open(&self) -> bool {
        unsafe { IsWindow(self.window) != 0 && (IsWindowVisible(self.window) != 0 || IsIconic(self.window) != 0) }
    }

    pub fn front(&self) {
        unsafe {
            ShowWindow(self.window, SW_SHOW);
            SetForegroundWindow(self.window);
        }
    }

    pub fn close(&self) {
        unsafe { ShowWindow(self.window, SW_HIDE) };
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // the view comes off the window before the window goes
        self.content.take();
        unsafe { DestroyWindow(self.window) };
    }
}
