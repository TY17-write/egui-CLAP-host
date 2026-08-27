//! モニタの GUI。**この段に入ってきた音**のスペクトルとラウドネスを出す。
//!
//! 測る部分は [`crate::monitor_meter`]。**ホストとはコードを共有していない**ので、
//! ホスト上部のマスターメーターと並べたときに一致すれば、独立した二つの実装が
//! 同じ音を見ていると言える。
//!
//! # 描き方
//!
//! Win32 + GDI で自前に描く。GUI のクレートを持ち込まないのは、**検証治具に
//! 依存を積みたくない**ため。`WM_PAINT` で完結するので、どの DAW に読ませても
//! 追加の作法が要らない。
//!
//! 再描画は `SetTimer` の `WM_TIMER`。ホストの UI スレッドが普通のメッセージ
//! ループを回していれば配送されるので、自前のポンプは要らない。
//!
//! **ちらつき止めに裏画面へ描いてから転送する。** 30fps で数十個の矩形を
//! 直接描くと目に見えて散らつく。

#![allow(unsafe_code)]

use crate::monitor_meter::{Meters, BANDS, CEIL_DB, FLOOR_DB, REFERENCE_LUFS, SILENCE_LUFS};
use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetStockObject, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, TextOutW, DEFAULT_GUI_FONT, HBRUSH, HDC, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, KillTimer, RegisterClassW,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, CREATESTRUCTW, GWLP_USERDATA,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_DESTROY, WM_ERASEBKGND,
    WM_NCCREATE, WM_PAINT, WM_TIMER, WNDCLASSW, WS_CHILD, WS_VISIBLE,
};

/// 窓の既定の大きさ。**上のスペクトルと下のラウドネスが両方入る最小限**
pub const WIDTH: u32 = 440;
pub const HEIGHT: u32 = 210;

/// 再描画タイマーの識別子と間隔。
///
/// ホストがエディタを開いている間の再描画間隔と揃えてある (33ms ≒ 30fps)。
/// メーターは目で追うものなので、これ以上速くしても読めない
const TIMER_ID: usize = 1;
const REDRAW_MS: u32 = 33;

/// ウィンドウクラス名 (UTF-16, NUL 終端) — "test_monitor_view\0"
const CLASS_NAME: &[u16] = &[
    0x74, 0x65, 0x73, 0x74, 0x5F, 0x6D, 0x6F, 0x6E, 0x69, 0x74, 0x6F, 0x72, 0x5F, 0x76, 0x69, 0x65,
    0x77, 0x00,
];

/// GDI の色 (0x00BBGGRR)。**ホストのテーマ (vim-hybrid 風) と同じ値**にしてある
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

const BG: COLORREF = rgb(0x1d, 0x1f, 0x21);
const BG_DARK: COLORREF = rgb(0x19, 0x1b, 0x1d);
const FG: COLORREF = rgb(0xc5, 0xc8, 0xc6);
const FG_DIM: COLORREF = rgb(0x70, 0x78, 0x80);
const GREEN: COLORREF = rgb(0xb5, 0xbd, 0x68);
const YELLOW: COLORREF = rgb(0xf0, 0xc6, 0x74);
const RED: COLORREF = rgb(0xcc, 0x66, 0x66);
const CYAN: COLORREF = rgb(0x8a, 0xbe, 0xb7);

/// 基準からこれだけ離れるまでは「合っている」として緑にする (ホストと同じ)
const TOLERANCE_LU: f32 = 1.0;
/// ラウドネスの目盛りに出す範囲。基準の -14 を挟んで上下に取る (ホストと同じ)
const SCALE_TOP: f32 = -4.0;
const SCALE_BOTTOM: f32 = -34.0;

/// オーディオスレッドと GUI の受け渡し口。
///
/// **活性化のたびに新しい輪を張る。** 使い回すと、止めて掛け直したときに
/// 前の輪の残りが混ざる。GUI は次の描画で新しい受け口に持ち替える。
#[derive(Default)]
pub struct Handoff {
    pending: Mutex<Option<rtrb::Consumer<f32>>>,
    /// 活性化時のサンプリングレート。0 は「まだ動いていない」
    sample_rate: AtomicU32,
}

impl Handoff {
    /// オーディオ側が活性化したときに呼ぶ。送り口を返す
    pub fn open(&self, sample_rate: u32, capacity: usize) -> rtrb::Producer<f32> {
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        self.sample_rate.store(sample_rate, Ordering::SeqCst);
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some(consumer);
        }
        producer
    }

    fn take(&self) -> Option<rtrb::Consumer<f32>> {
        self.pending.lock().ok()?.take()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::SeqCst)
    }
}

/// 窓が指す表示状態。**窓より長生きする** (開け閉めで作り直さない)
pub struct MonitorView {
    handoff: std::sync::Arc<Handoff>,
    consumer: Option<rtrb::Consumer<f32>>,
    meters: Meters,
    last_tick: Instant,
}

impl MonitorView {
    pub fn new(handoff: std::sync::Arc<Handoff>) -> Self {
        Self {
            handoff,
            consumer: None,
            // 活性化するまでレートは分からない。最初の取り込みで作り直される
            meters: Meters::new(48_000),
            last_tick: Instant::now(),
        }
    }

    /// 1フレーム分を取り込む
    fn tick(&mut self) {
        // 掛け直されていれば新しい受け口へ持ち替える
        if let Some(consumer) = self.handoff.take() {
            self.consumer = Some(consumer);
            self.meters.reset();
        }

        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32();
        self.last_tick = now;

        let sample_rate = self.handoff.sample_rate();
        if let Some(consumer) = self.consumer.as_mut() {
            self.meters.drain(consumer, sample_rate, dt);
        }
    }

    /// 動いているか (活性化していれば true)
    fn running(&self) -> bool {
        self.consumer.is_some() && self.handoff.sample_rate() != 0
    }
}

/// モニタの窓。ホストから渡された親の中に子として作る
pub struct MonitorWindow {
    hwnd: HWND,
}

impl MonitorWindow {
    /// 親の中に子ウィンドウを作る。`view` は窓より長生きすること
    ///
    /// # Safety
    ///
    /// `parent` が有効な HWND で、`view` が窓を壊すまで生きていること。
    pub unsafe fn open(
        parent: HWND,
        view: *const RefCell<MonitorView>,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        register_class();

        let hwnd = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            width as i32,
            height as i32,
            parent,
            null_mut(),
            GetModuleHandleW(null()),
            view as *const c_void,
        );
        if hwnd.is_null() {
            return None;
        }
        SetTimer(hwnd, TIMER_ID, REDRAW_MS, None);
        Some(Self { hwnd })
    }

    pub fn resize(&self, width: u32, height: u32) {
        unsafe {
            SetWindowPos(
                self.hwnd,
                null_mut(),
                0,
                0,
                width as i32,
                height as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe {
            ShowWindow(self.hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }
}

impl Drop for MonitorWindow {
    fn drop(&mut self) {
        unsafe {
            KillTimer(self.hwnd, TIMER_ID);
            DestroyWindow(self.hwnd);
        }
    }
}

fn register_class() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(null()),
            hIcon: null_mut(),
            hCursor: null_mut(),
            // **背景は描かない。** 全面を自前で塗るので、消させるとちらつく
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        RegisterClassW(&class);
    });
}

/// GWLP_USERDATA から表示状態を取り出す (WM_NCCREATE 前は None)
unsafe fn view_of<'a>(hwnd: HWND) -> Option<&'a RefCell<MonitorView>> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<MonitorView>;
    ptr.as_ref()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = lparam as *const CREATESTRUCTW;
            if let Some(create) = create.as_ref() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER => {
            if let Some(view) = view_of(hwnd) {
                if let Ok(mut view) = view.try_borrow_mut() {
                    view.tick();
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        // 全面を自前で塗るので、消させない (ちらつきの元)
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(view) = view_of(hwnd) {
                if let Ok(view) = view.try_borrow() {
                    paint_buffered(hdc, &ps.rcPaint, &view);
                }
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ID);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 裏画面へ描いてから一度に転送する (ちらつき止め)
unsafe fn paint_buffered(hdc: HDC, area: &RECT, view: &MonitorView) {
    let width = area.right - area.left;
    let height = area.bottom - area.top;
    if width <= 0 || height <= 0 {
        return;
    }

    let back_dc = CreateCompatibleDC(hdc);
    let bitmap = CreateCompatibleBitmap(hdc, width, height);
    let old = SelectObject(back_dc, bitmap as _);

    paint(back_dc, width, height, view);

    BitBlt(
        hdc, area.left, area.top, width, height, back_dc, 0, 0, SRCCOPY,
    );

    SelectObject(back_dc, old);
    DeleteObject(bitmap as _);
    DeleteDC(back_dc);
}

/// 中身を描く。`width` / `height` は描き先の大きさ
unsafe fn paint(hdc: HDC, width: i32, height: i32, view: &MonitorView) {
    fill(hdc, 0, 0, width, height, BG);
    SetBkMode(hdc, TRANSPARENT as i32);
    SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT) as _);

    let running = view.running();

    // ---- 見出し ----
    text(
        hdc,
        12,
        8,
        "この段に入ってきた音 (出力ポートを持たない段)",
        FG_DIM,
    );

    // ---- スペクトル ----
    let top = 30;
    let bar_area_h = 96;
    let left = 12;
    let area_w = width - 24;
    fill(hdc, left, top, area_w, bar_area_h, BG_DARK);

    let levels = view.meters.spectrum_levels();
    let span = CEIL_DB - FLOOR_DB;
    let bar_w = (area_w as f32 / BANDS as f32).max(1.0);
    for (index, level) in levels.iter().enumerate() {
        // 下限からの高さの割合。**止まっているときは描かない** (残像に見えるため)
        let ratio = if running {
            ((level - FLOOR_DB) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if ratio <= 0.0 {
            continue;
        }
        let h = (ratio * bar_area_h as f32).round() as i32;
        let x = left + (index as f32 * bar_w).round() as i32;
        let w = (bar_w.round() as i32 - 1).max(1);
        // 低い帯はシアン、高い帯は黄 (ホストと同じ配色)
        let hue = index as f32 / (BANDS - 1) as f32;
        fill(hdc, x, top + bar_area_h - h, w, h, lerp(CYAN, YELLOW, hue));
    }

    // ---- ラウドネス ----
    let momentary = view.meters.momentary_lufs();
    let short_term = view.meters.short_term_lufs();
    let integrated = view.meters.integrated_lufs();

    let scale_top = top + bar_area_h + 16;
    text(hdc, left, scale_top, "M", FG_DIM);
    text(hdc, left + 20, scale_top, &reading(momentary, running), FG);
    text(hdc, left + 90, scale_top, "S", FG_DIM);
    text(
        hdc,
        left + 110,
        scale_top,
        &reading(short_term, running),
        FG,
    );
    text(hdc, left + 180, scale_top, "I", FG);
    text(
        hdc,
        left + 200,
        scale_top,
        &reading(integrated, running),
        FG,
    );
    text(
        hdc,
        left + 280,
        scale_top,
        &format!("基準 {REFERENCE_LUFS:.0} LUFS"),
        FG_DIM,
    );

    // 目盛り。Integrated の位置に印を出す
    let bar_top = scale_top + 24;
    let bar_h = 12;
    fill(hdc, left, bar_top, area_w, bar_h, BG_DARK);
    if running && integrated > SILENCE_LUFS {
        let ratio = ((integrated - SCALE_BOTTOM) / (SCALE_TOP - SCALE_BOTTOM)).clamp(0.0, 1.0);
        let w = (ratio * area_w as f32).round() as i32;
        let color = if (integrated - REFERENCE_LUFS).abs() <= TOLERANCE_LU {
            GREEN
        } else if integrated > REFERENCE_LUFS {
            RED
        } else {
            YELLOW
        };
        fill(hdc, left, bar_top, w.max(1), bar_h, color);
    }
    // 基準の位置に縦線
    let reference = ((REFERENCE_LUFS - SCALE_BOTTOM) / (SCALE_TOP - SCALE_BOTTOM)).clamp(0.0, 1.0);
    let x = left + (reference * area_w as f32).round() as i32;
    fill(hdc, x, bar_top - 2, 1, bar_h + 4, FG);

    if !running {
        text(hdc, left, bar_top + bar_h + 8, "(停止中)", FG_DIM);
    }
}

/// 読み値の文字列。無音は「-∞」
fn reading(lufs: f32, running: bool) -> String {
    if !running || lufs <= SILENCE_LUFS {
        "  -∞".to_string()
    } else {
        format!("{lufs:6.1}")
    }
}

/// 2色の間を線形に混ぜる
fn lerp(from: COLORREF, to: COLORREF, t: f32) -> COLORREF {
    let mix = |shift: u32| {
        let a = ((from >> shift) & 0xFF) as f32;
        let b = ((to >> shift) & 0xFF) as f32;
        ((a + (b - a) * t.clamp(0.0, 1.0)).round() as u32) << shift
    };
    mix(0) | mix(8) | mix(16)
}

/// 矩形を塗る
unsafe fn fill(hdc: HDC, x: i32, y: i32, w: i32, h: i32, color: COLORREF) {
    let rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let brush: HBRUSH = CreateSolidBrush(color);
    FillRect(hdc, &rect, brush);
    DeleteObject(brush as _);
}

/// 文字を描く
unsafe fn text(hdc: HDC, x: i32, y: i32, value: &str, color: COLORREF) {
    let wide: Vec<u16> = value.encode_utf16().collect();
    SetTextColor(hdc, color);
    TextOutW(hdc, x, y, wide.as_ptr(), wide.len() as i32);
}
