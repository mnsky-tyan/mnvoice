// Ethereal floating gas-fluid orb (48px) for mnvoice - the Windows surface.
// The simulation that paints the pixels is platform-neutral and lives in
// `orb_render`; this module is only where those pixels go on Windows: a
// Win32 layered window (UpdateLayeredWindow) with 32-bit premultiplied ARGB.
// Sits unobtrusively right at the bottom edge of the screen, 2px above the
// taskbar. Click-through, non-activating, zero interference with active apps.
// The Linux surface is the same render blitted to a Wayland layer-shell
// surface (`platform/linux_orb.rs`).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::orb_render::{OrbState, ORB_HEIGHT, ORB_WIDTH};
use crate::windows_app::log;

const ORB_CLASS_NAME: PCWSTR = w!("mnvoiceOrbClass");

pub struct Orb {
    hwnd: HWND,
    dc_mem: HDC,
    bitmap: HBITMAP,
    bits: *mut u32,
    state: OrbState,
    frame: u32,
    visible: bool,
    color: (f32, f32, f32),
    fluid_level: f32,
    /// Cached bottom-center position; refreshed by `reposition`, read per
    /// frame so painting does not call SystemParametersInfoW 30+ times a second.
    pos: (i32, i32),
}

/// The orb's window class needs no message handling of its own - every
/// message goes straight to `DefWindowProcW` - but the registration demands a
/// procedure, so this is a one-call forwarder.
///
/// Bare `extern "system" fn` with an `unsafe` block at the call, the same shape
/// `windows_app.rs`'s `wndproc` uses. Both can be that shape because `WNDPROC`
/// is `Option<unsafe extern "system" fn(..)>` and a safe `fn` coerces into it -
/// the windows crate does not require `unsafe` on the declaration. The two
/// procedures used to be declared differently (`unsafe extern` here, bare
/// there), which told a reader nothing about which form the crate demands, so
/// they match now.
extern "system" fn orb_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl Orb {
    pub fn new(
        instance: HINSTANCE,
        color: (f32, f32, f32),
        fluid_level: f32,
    ) -> Result<Self, String> {
        unsafe {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(orb_wndproc),
                hInstance: instance,
                lpszClassName: ORB_CLASS_NAME,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            let ex_style = WS_EX_LAYERED
                | WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_TRANSPARENT
                | WS_EX_NOACTIVATE;

            let (pos_x, pos_y) = calc_position();

            let hwnd = match CreateWindowExW(
                ex_style,
                ORB_CLASS_NAME,
                w!("mnvoiceOrb"),
                WS_POPUP,
                pos_x,
                pos_y,
                ORB_WIDTH as i32,
                ORB_HEIGHT as i32,
                None,
                None,
                instance,
                None,
            ) {
                Ok(h) => h,
                Err(e) => return Err(format!("CreateWindowExW for orb failed: {e}")),
            };

            let screen_dc = GetDC(HWND::default());
            let dc_mem = CreateCompatibleDC(screen_dc);
            let _ = ReleaseDC(HWND::default(), screen_dc);
            if dc_mem.0.is_null() {
                let _ = DestroyWindow(hwnd);
                return Err("CreateCompatibleDC failed".into());
            }

            // A BI_BITFIELDS header with an explicit alpha mask is required here.
            // Plain BI_RGB at 32bpp gives a DGB with no alpha, and UpdateLayeredWindow
            // then composites a fully transparent surface - the orb is invisible.
            let bmi = BITMAPV5HEADER {
                bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
                bV5Width: ORB_WIDTH as i32,
                bV5Height: -(ORB_HEIGHT as i32), // top-down DIB
                bV5Planes: 1,
                bV5BitCount: 32,
                bV5Compression: BI_BITFIELDS,
                bV5RedMask: 0x00FF_0000,
                bV5GreenMask: 0x0000_FF00,
                bV5BlueMask: 0x0000_00FF,
                bV5AlphaMask: 0xFF00_0000,
                ..Default::default()
            };

            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(
                dc_mem,
                &bmi as *const BITMAPV5HEADER as *const BITMAPINFO,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                HANDLE::default(),
                0,
            ) {
                Ok(bmp) => bmp,
                Err(e) => {
                    let _ = DeleteDC(dc_mem);
                    let _ = DestroyWindow(hwnd);
                    return Err(format!("CreateDIBSection failed: {e}"));
                }
            };

            // Checked like every other Win32 call in this function: this is
            // what makes the DIB section the memory DC's surface, and a
            // refused selection would leave `dc_mem` on its default 1x1
            // bitmap - so the orb would composite a blank surface for the life
            // of the process while render_frame drew into a bitmap nothing
            // displays, with no diagnostic anywhere.
            //
            // `SelectObject` returns `HGDIOBJ`, not a `Result`: the windows
            // crate does not convert this one. Failure is `HGDI_ERROR`, whose
            // value is all ones - a valid handle is never that, because GDI
            // handles are small non-negative integers.
            let previous = SelectObject(dc_mem, bitmap);
            if previous.0 as isize == -1 {
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(dc_mem);
                let _ = DestroyWindow(hwnd);
                return Err("SelectObject refused the DIB section".into());
            }

            Ok(Self {
                hwnd,
                dc_mem,
                bitmap,
                bits: bits_ptr as *mut u32,
                state: OrbState::Recording,
                frame: 0,
                visible: false,
                color,
                fluid_level,
                pos: (pos_x, pos_y),
            })
        }
    }

    pub fn show(&mut self, state: OrbState) {
        self.state = state;
        self.frame = 0;
        self.reposition();
        self.render_frame();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
        self.visible = true;
    }

    pub fn set_state(&mut self, state: OrbState) {
        self.state = state;
        if self.visible {
            self.render_frame();
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        self.frame = self.frame.wrapping_add(1);
        self.render_frame();
    }

    fn reposition(&mut self) {
        // Re-read the work area here (each session start), not per frame:
        // render_frame paints in place, so its position is whatever this set.
        self.pos = calc_position();
        let (pos_x, pos_y) = self.pos;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                pos_x,
                pos_y,
                ORB_WIDTH as i32,
                ORB_HEIGHT as i32,
                SWP_NOACTIVATE | SWP_NOSIZE,
            );
        }
    }

    fn render_frame(&mut self) {
        if self.bits.is_null() {
            return;
        }
        let total_pixels = (ORB_WIDTH * ORB_HEIGHT) as usize;
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bits, total_pixels) };

        // Both states render; which one differs only in the drift speed, and
        // `render_gas_fluid` decides that from the state itself.
        crate::orb_render::render_gas_fluid(
            self.frame,
            buf,
            self.state,
            self.color,
            self.fluid_level,
        );

        unsafe {
            let (pos_x, pos_y) = self.pos;
            let pt_dst = POINT { x: pos_x, y: pos_y };
            let size = SIZE {
                cx: ORB_WIDTH as i32,
                cy: ORB_HEIGHT as i32,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                HDC::default(),
                Some(&pt_dst),
                Some(&size),
                self.dc_mem,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for Orb {
    fn drop(&mut self) {
        self.hide();
        unsafe {
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc_mem.0.is_null() {
                let _ = DeleteDC(self.dc_mem);
            }
            if !self.hwnd.0.is_null() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn calc_position() -> (i32, i32) {
    let mut work_area = RECT::default();
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work_area as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    // A refused geometry read leaves `work_area` all zeros, and the arithmetic
    // below would then place the orb at (-24, 0) - off the top-left corner and
    // invisible, which reads as "the indicator is broken" rather than "the
    // screen size is unknown". Fall back to the primary monitor's metrics and
    // say which source was used, so a failure is diagnosable instead of
    // silent.
    if read.is_err() {
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if w > 0 && h > 0 {
            log(&format!(
                "orb: SPI_GETWORKAREA refused, using primary screen metrics {w}x{h}"
            ));
            return (x_for(0, w), y_for(0, h));
        }
        log("orb: no screen geometry available, using the top-left corner");
        return (0, 0);
    }
    (
        x_for(work_area.left, work_area.right),
        y_for(work_area.top, work_area.bottom),
    )
}

/// Centres the orb horizontally in `[left, right)`.
fn x_for(left: i32, right: i32) -> i32 {
    left + ((right - left) - ORB_WIDTH as i32) / 2
}

/// Anchors the orb at the bottom of `[top, bottom)`, 2px above the taskbar.
fn y_for(top: i32, bottom: i32) -> i32 {
    (bottom - ORB_HEIGHT as i32 - 2).max(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orb_render::{body_pixels, render_gas_fluid};

    /// The Windows window exists so the shared render has somewhere to go; this
    /// file's own assertions are about the window plumbing, and the look itself
    /// is pinned in `orb_render`'s tests. What belongs here is the contract the
    /// surface adds: the buffer the window paints is premultiplied ARGB packed
    /// low-byte-first (BGRA in memory), which UpdateLayeredWindow requires.
    #[test]
    fn orb_dimensions_are_square_and_match_the_shared_render() {
        assert_eq!(ORB_WIDTH, ORB_HEIGHT, "the orb is a square");
        assert_eq!(ORB_WIDTH, crate::orb_render::ORB_WIDTH);
    }
}
