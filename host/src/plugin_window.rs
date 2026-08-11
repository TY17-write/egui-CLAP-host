//! プラグイン GUI 埋め込み用の Win32 ネイティブウィンドウ。
//!
//! eframe (winit) のメッセージループは同一スレッド上の全ウィンドウにメッセージを
//! 配送するため、メインスレッドで CreateWindowExW したウィンドウは追加のポンプなしで
//! 動作する。WndProc からの通知は crossbeam チャンネルで egui 側へ送る。

#![allow(unsafe_code)]

use crate::host::MainThreadMessage;
use crossbeam_channel::Sender;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindow,
    GetWindowLongPtrW, LoadCursorW, RegisterClassW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, GWL_STYLE, GW_CHILD, IDC_ARROW, SIZE_MINIMIZED,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, WM_CLOSE,
    WM_DESTROY, WM_NCCREATE, WM_SIZE, WNDCLASSW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW,
    WS_THICKFRAME,
};

/// ウィンドウクラス名 (UTF-16, NUL 終端)
const CLASS_NAME: &[u16] = &[
    0x63, 0x6C, 0x61, 0x70, 0x5F, 0x68, 0x6F, 0x73, 0x74, 0x5F, 0x70, 0x6C, 0x75, 0x67, 0x69,
    0x6E, 0x5F, 0x77, 0x69, 0x6E, 0x64, 0x6F, 0x77, 0x00,
]; // "clap_host_plugin_window\0"

/// WndProc から参照される状態。GWLP_USERDATA に Box の生ポインタとして格納し、
/// WM_DESTROY で解放する。
struct WindowState {
    sender: Sender<MainThreadMessage>,
}

/// プラグイン GUI を埋め込むためのネイティブウィンドウ
pub struct PluginWindow {
    hwnd: HWND,
}

impl PluginWindow {
    /// ウィンドウを作成して表示する。サイズはクライアント領域の物理ピクセル。
    pub fn create(
        title: &str,
        client_width: u32,
        client_height: u32,
        resizable: bool,
        sender: Sender<MainThreadMessage>,
    ) -> Option<Self> {
        ensure_class_registered();

        let style = if resizable {
            WS_OVERLAPPEDWINDOW
        } else {
            WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX)
        };

        // クライアント領域が指定サイズになるよう外形を計算する
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client_width as i32,
            bottom: client_height as i32,
        };
        unsafe { AdjustWindowRectEx(&mut rect, style, 0, 0) };

        let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let state = Box::into_raw(Box::new(WindowState { sender }));

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                CLASS_NAME.as_ptr(),
                title_wide.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                state as *const c_void,
            )
        };

        if hwnd.is_null() {
            // WM_NCCREATE に到達していなければ WindowState は未解放
            unsafe { drop(Box::from_raw(state)) };
            return None;
        }

        unsafe { ShowWindow(hwnd, SW_SHOW) };

        Some(Self { hwnd })
    }

    /// プラグインに渡す HWND
    /// (CLAP は `gui.set_parent`、VST3 は `IPlugView::attached` の行き先)
    pub fn hwnd(&self) -> *mut c_void {
        self.hwnd
    }

    /// プラグインが貼り付けた子ウィンドウを列挙して、1つずつ状態を文字列にする。
    ///
    /// 「描かれているのに一部しか押せない」を追うためのもの。描画と入力は別物で、
    /// **無効化された (`EnableWindow(false)`) 子は、描かれるが入力を受け取らない**。
    /// 位置がずれている子も、見た目どおりの場所を押しても当たらない。
    /// どちらも、この一覧に出れば一目で分かる。
    ///
    /// 位置と大きさはこの窓のクライアント領域を基準にした値。
    pub fn describe_embedded_children(&self) -> Vec<String> {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetClassNameW, GetWindowRect, IsWindowVisible, GW_HWNDNEXT,
        };

        /// 異常な数の子がぶら下がっていても止まらないように上限を置く
        const MAX_CHILDREN: usize = 16;

        let mut described = Vec::new();
        unsafe {
            let mut parent = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(self.hwnd, &mut parent) == 0 {
                return described;
            }

            let mut child = GetWindow(self.hwnd, GW_CHILD);
            while !child.is_null() && described.len() < MAX_CHILDREN {
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                let mut class = [0u16; 64];
                let length = GetClassNameW(child, class.as_mut_ptr(), class.len() as i32);
                let class = String::from_utf16_lossy(&class[..length.max(0) as usize]);

                if GetWindowRect(child, &mut rect) != 0 {
                    described.push(format!(
                        "{class}: {}x{} @({},{}) 有効={} 表示={}",
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        rect.left - parent.left,
                        rect.top - parent.top,
                        IsWindowEnabled(child) != 0,
                        IsWindowVisible(child) != 0,
                    ));
                } else {
                    described.push(format!("{class}: 位置を取得できない"));
                }

                child = GetWindow(child, GW_HWNDNEXT);
            }
        }
        described
    }

    /// プラグインが貼り付けた中身 (子ウィンドウ) のクライアント領域サイズ。
    ///
    /// CLAP も VST3 も、埋め込みではこの窓に**子ウィンドウとしてぶら下がる**ので、
    /// ここが実際に描かれて入力を受け取る範囲になる。[`client_size`](Self::client_size)
    /// と食い違っていれば、窓の一部に中身が無い (= その部分をクリックしても
    /// 何も起きない) ことになる。
    ///
    /// 貼り付け前や、子を作らない実装では `None`。
    pub fn embedded_child_size(&self) -> Option<(u32, u32)> {
        unsafe {
            let child = GetWindow(self.hwnd, GW_CHILD);
            if child.is_null() {
                return None;
            }
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(child, &mut rect) == 0 {
                return None;
            }
            Some((
                (rect.right - rect.left).max(0) as u32,
                (rect.bottom - rect.top).max(0) as u32,
            ))
        }
    }

    /// クライアント領域が指定サイズになるようウィンドウをリサイズする
    pub fn resize_client(&self, width: u32, height: u32) {
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            AdjustWindowRectEx(&mut rect, style, 0, 0);
            SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// 枠のリサイズ可否を後から変える。
    ///
    /// VST3 では「リサイズできるか」を開く前に聞くと、そのためだけに使い捨ての
    /// エディタが作られてしまう (重い音源だと数秒かかる)。貼り付けたあとに聞いて
    /// ここで直すほうが速い。
    pub fn set_resizable(&self, resizable: bool) {
        unsafe {
            let current = GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32;
            let updated = if resizable {
                current | WS_THICKFRAME | WS_MAXIMIZEBOX
            } else {
                current & !(WS_THICKFRAME | WS_MAXIMIZEBOX)
            };
            if updated == current {
                return;
            }
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, updated as isize);
            // 枠を描き直させる。位置と大きさは触らない
            SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }

    /// 現在のクライアント領域サイズ
    pub fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetClientRect(self.hwnd, &mut rect) };
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }
}

impl Drop for PluginWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_null() {
            unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = null_mut();
        }
    }
}

/// ウィンドウクラスを一度だけ登録する
fn ensure_class_registered() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(null()),
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        RegisterClassW(&class);
    });
}

/// GWLP_USERDATA から WindowState を取り出す (WM_NCCREATE 前は None)
unsafe fn state_of(hwnd: HWND) -> Option<&'static WindowState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
    ptr.as_ref()
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // CreateWindowExW の lpParam から WindowState を受け取り GWLP_USERDATA に保存
            let create = lparam as *const CREATESTRUCTW;
            let state = (*create).lpCreateParams as *mut WindowState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            // 破棄はアプリ側 (プラグイン GUI の destroy 後) に任せる
            if let Some(state) = state_of(hwnd) {
                let _ = state.sender.send(MainThreadMessage::PluginWindowClosed);
            }
            0
        }
        WM_SIZE => {
            if wparam != SIZE_MINIMIZED as usize {
                let width = (lparam & 0xFFFF) as u32;
                let height = ((lparam >> 16) & 0xFFFF) as u32;
                if let Some(state) = state_of(hwnd) {
                    let _ = state
                        .sender
                        .send(MainThreadMessage::PluginWindowResized { width, height });
                }
            }
            0
        }
        WM_DESTROY => {
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut WindowState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
