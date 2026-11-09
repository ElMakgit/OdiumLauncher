//! Liquid glass for the Windows 7 theme.
//!
//! A flat egui swap chain cannot blur the desktop by itself, and Windows 11
//! disables its acrylic backdrop when the system-wide "Transparency effects"
//! toggle is off. To keep the launcher glassy regardless of that setting, the
//! desktop is captured once before the window appears, blurred off the UI
//! thread, and painted screen-aligned behind the translucent Aero surfaces.
//! DWM acrylic is still requested for the native frame: on systems where it is
//! available it makes the title bar glassy as well.

use std::sync::mpsc;
use std::time::Duration;

/// Extracts the native window handle from eframe. Returns `None` on platforms
/// where a Win32 HWND does not exist.
pub fn window_handle(frame: &eframe::Frame) -> Option<isize> {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

        let handle = frame.window_handle().ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = frame;
        None
    }
}

/// Enables or disables the DWM acrylic backdrop for the given window.
pub fn set_enabled(hwnd: isize, enabled: bool) {
    #[cfg(windows)]
    imp::set_enabled(hwnd, enabled);
    #[cfg(not(windows))]
    {
        let _ = (hwnd, enabled);
    }
}

/// Position and size of the window's client area in physical screen pixels.
#[derive(Debug, Clone, Copy)]
pub struct ClientGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn client_geometry(hwnd: isize) -> Option<ClientGeometry> {
    #[cfg(windows)]
    {
        imp::client_geometry(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// A blurred screenshot of the desktop, ready to be painted behind the UI.
///
/// The capture itself happens synchronously in [`Backdrop::capture_now`], before
/// the launcher window exists, so the launcher cannot photograph itself. The
/// blur runs on a worker thread and is uploaded to the GPU by [`Self::poll`].
pub struct Backdrop {
    rx: Option<mpsc::Receiver<Snapshot>>,
    texture: Option<egui::TextureHandle>,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
}

/// Shows the window again on `Drop`, so a recapture cannot leave the launcher
/// hidden when the capture worker fails or panics.
#[cfg(windows)]
struct RestoreWindow(isize);

#[cfg(windows)]
impl Drop for RestoreWindow {
    fn drop(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNA};

        // SAFETY: this is the window `recapture` hid moments ago; SW_SHOWNA
        // shows it without stealing focus from the player.
        unsafe {
            ShowWindow(self.0 as windows_sys::Win32::Foundation::HWND, SW_SHOWNA);
        }
    }
}

struct Snapshot {
    pixels: Vec<u8>,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
}

impl Backdrop {
    /// An inert backdrop for opaque themes: no screenshot is taken and no blur
    /// worker or GPU texture is allocated.
    pub fn empty() -> Self {
        Self {
            rx: None,
            texture: None,
            origin_x: 0,
            origin_y: 0,
            width: 0,
            height: 0,
        }
    }

    /// Grabs the screen right now and starts blurring it in the background.
    /// Call this before creating the launcher window.
    pub fn capture_now() -> Self {
        #[cfg(not(windows))]
        {
            // Other platforms never capture the desktop: the glass falls back
            // to transparent surfaces without a blurred backdrop.
            Self::empty()
        }
        #[cfg(windows)]
        {
            let mut backdrop = Self {
                rx: None,
                texture: None,
                origin_x: 0,
                origin_y: 0,
                width: 0,
                height: 0,
            };

            if let Some(snapshot) = capture_screen() {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut snapshot = snapshot;
                    let width = snapshot.width as usize;
                    let height = snapshot.height as usize;
                    box_blur(&mut snapshot.pixels, width, height, 10);
                    let _ = tx.send(snapshot);
                });
                backdrop.rx = Some(rx);
                tracing::info!("desktop snapshot captured for the Windows 7 glass");
            } else {
                tracing::warn!("desktop capture failed; glass falls back to DWM acrylic");
            }

            backdrop
        }
    }

    /// Recaptures the desktop after a live switch to the glass theme. The
    /// launcher window is hidden immediately; the 140 ms settle, GDI capture
    /// and blur all run on a worker thread. The old texture stays visible
    /// until the new snapshot is ready.
    ///
    /// The worker restores the window itself instead of leaving that to
    /// [`Self::poll`]: hiding the window can stall the egui redraw loop, in
    /// which case `update` (and therefore `poll`) is never called again and
    /// the launcher would stay invisible forever.
    pub fn recapture(&mut self, hwnd: Option<isize>) {
        #[cfg(windows)]
        if let Some(hwnd) = hwnd {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

            tracing::info!("recapturing the desktop for the Windows 7 glass");
            let handle = hwnd as windows_sys::Win32::Foundation::HWND;
            // SAFETY: ShowWindow only toggles this window's visibility.
            unsafe {
                ShowWindow(handle, SW_HIDE);
            }

            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                // Restores the window exactly once, whatever happens below:
                // on success, when the capture fails and even on a panic.
                let _restore = RestoreWindow(hwnd);
                std::thread::sleep(Duration::from_millis(140));
                if let Some(mut snapshot) = capture_screen() {
                    let width = snapshot.width as usize;
                    let height = snapshot.height as usize;
                    box_blur(&mut snapshot.pixels, width, height, 10);
                    let _ = tx.send(snapshot);
                }
            });
            self.rx = Some(rx);
            return;
        }
        #[cfg(not(windows))]
        let _ = hwnd;
        *self = Self::capture_now();
    }

    /// Uploads the blurred snapshot once the worker finishes. Safe to call on
    /// every frame; it is a no-op after the texture exists.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(snapshot) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [snapshot.width as usize, snapshot.height as usize],
                    &snapshot.pixels,
                );
                self.texture = Some(ctx.load_texture(
                    "aero_desktop_snapshot",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
                self.origin_x = snapshot.origin_x;
                self.origin_y = snapshot.origin_y;
                self.width = snapshot.width;
                self.height = snapshot.height;
                tracing::info!(
                    width = snapshot.width,
                    height = snapshot.height,
                    "blurred desktop backdrop uploaded"
                );
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
                ctx.request_repaint_after(Duration::from_millis(40));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                tracing::warn!("desktop blur worker stopped before producing a frame");
            }
        }
    }

    /// Paints the screen-aligned blurred desktop behind all egui layers.
    pub fn paint(&self, ctx: &egui::Context, hwnd: isize) {
        let (Some(texture), Some(client)) = (&self.texture, client_geometry(hwnd)) else {
            return;
        };
        if self.width == 0 || self.height == 0 {
            return;
        }

        let x0 = (client.x - self.origin_x) as f32 / self.width as f32;
        let y0 = (client.y - self.origin_y) as f32 / self.height as f32;
        let x1 = (client.x + client.width as i32 - self.origin_x) as f32 / self.width as f32;
        let y1 = (client.y + client.height as i32 - self.origin_y) as f32 / self.height as f32;

        ctx.layer_painter(egui::LayerId::background()).image(
            texture.id(),
            ctx.screen_rect(),
            egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)),
            egui::Color32::WHITE,
        );
    }
}

#[cfg(windows)]
fn capture_screen() -> Option<Snapshot> {
    use core::ffi::c_void;
    use std::ptr::null_mut;

    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    let (origin_x, origin_y, width, height) = unsafe {
        let virtual_width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let virtual_height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        // Very large multi-monitor walls can exceed the maximum texture size.
        if (1..=8192).contains(&virtual_width) && (1..=8192).contains(&virtual_height) {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                virtual_width,
                virtual_height,
            )
        } else {
            (
                0,
                0,
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        }
    };
    if width <= 0 || height <= 0 {
        return None;
    }

    let screen_dc = unsafe { GetDC(null_mut()) };
    if screen_dc.is_null() {
        return None;
    }
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc.is_null() {
        unsafe {
            ReleaseDC(null_mut(), screen_dc);
        }
        return None;
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
    if bitmap.is_null() {
        unsafe {
            DeleteDC(memory_dc);
            ReleaseDC(null_mut(), screen_dc);
        }
        return None;
    }

    let old_bitmap = unsafe { SelectObject(memory_dc, bitmap) };
    let copied = unsafe {
        BitBlt(
            memory_dc,
            0,
            0,
            width,
            height,
            screen_dc,
            origin_x,
            origin_y,
            SRCCOPY | CAPTUREBLT,
        )
    };

    let pixel_count = width as usize * height as usize;
    let mut pixels = vec![0u8; pixel_count * 4];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height requests a top-down buffer.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbReserved: 0,
        }],
    };
    let scanned_lines = if copied != 0 {
        unsafe {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast::<c_void>(),
                &mut info,
                DIB_RGB_COLORS,
            )
        }
    } else {
        0
    };

    unsafe {
        if !old_bitmap.is_null() {
            SelectObject(memory_dc, old_bitmap);
        }
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(null_mut(), screen_dc);
    }

    if scanned_lines == 0 {
        return None;
    }

    // GDI hands out BGRA; egui wants RGBA.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }

    Some(Snapshot {
        pixels,
        origin_x,
        origin_y,
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(windows)]
fn box_blur(pixels: &mut [u8], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    let mut scratch = vec![0u8; pixels.len()];
    // Three box passes approximate a Gaussian closely enough for glass and are
    // linear in the pixel count, unlike a real Gaussian kernel.
    for _ in 0..3 {
        blur_horizontal(pixels, &mut scratch, width, height, radius);
        blur_vertical(&scratch, pixels, width, height, radius);
    }
}

#[cfg(windows)]
fn blur_horizontal(src: &[u8], dst: &mut [u8], width: usize, height: usize, radius: usize) {
    let window = (radius * 2 + 1) as u32;
    for y in 0..height {
        let row = y * width * 4;
        let mut sum = [0u32; 3];

        // Initial window for x = 0 covers [-radius, radius] with clamped edges.
        let first = row;
        for _ in 0..=radius {
            for channel in 0..3 {
                sum[channel] += src[first + channel] as u32;
            }
        }
        for x in 1..=radius {
            let offset = row + x.min(width - 1) * 4;
            for channel in 0..3 {
                sum[channel] += src[offset + channel] as u32;
            }
        }

        for x in 0..width {
            let offset = row + x * 4;
            for channel in 0..3 {
                dst[offset + channel] = (sum[channel] / window) as u8;
            }
            dst[offset + 3] = 255;

            let leaving = row + x.saturating_sub(radius) * 4;
            let entering = row + (x + radius + 1).min(width - 1) * 4;
            for channel in 0..3 {
                sum[channel] += src[entering + channel] as u32;
                sum[channel] -= src[leaving + channel] as u32;
            }
        }
    }
}

#[cfg(windows)]
fn blur_vertical(src: &[u8], dst: &mut [u8], width: usize, height: usize, radius: usize) {
    let window = (radius * 2 + 1) as u32;
    let stride = width * 4;
    for x in 0..width {
        let column = x * 4;
        let mut sum = [0u32; 3];

        let first = column;
        for _ in 0..=radius {
            for channel in 0..3 {
                sum[channel] += src[first + channel] as u32;
            }
        }
        for y in 1..=radius {
            let offset = y.min(height - 1) * stride + column;
            for channel in 0..3 {
                sum[channel] += src[offset + channel] as u32;
            }
        }

        for y in 0..height {
            let offset = y * stride + column;
            for channel in 0..3 {
                dst[offset + channel] = (sum[channel] / window) as u8;
            }
            dst[offset + 3] = 255;

            let leaving = y.saturating_sub(radius) * stride + column;
            let entering = (y + radius + 1).min(height - 1) * stride + column;
            for channel in 0..3 {
                sum[channel] += src[entering + channel] as u32;
                sum[channel] -= src[leaving + channel] as u32;
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, InvalidateRect};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::UI::Controls::MARGINS;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        gradient_color: u32,
        animation_id: u32,
    }

    #[repr(C)]
    struct WindowCompositionAttributeData {
        attribute: u32,
        data: *mut c_void,
        size_of_data: usize,
    }

    /// `DWMWA_SYSTEMBACKDROP_TYPE` from dwmapi.h (Windows 11 22H2+).
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMSBT_NONE: i32 = 1;
    /// Acrylic: the same transient backdrop the Start menu and flyouts use.
    const DWMSBT_TRANSIENTWINDOW: i32 = 3;

    /// `WCA_ACCENT_POLICY` from the undocumented user32 composition API.
    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_DISABLED: u32 = 0;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

    /// AABBGGRR. A light 25% white tint keeps the blur readable without
    /// washing the glass out.
    const ACRYLIC_TINT: u32 = 0x40FF_FFFF;

    pub fn set_enabled(hwnd: isize, enabled: bool) {
        let hwnd = hwnd as HWND;
        if enabled {
            let margins = MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            };
            let extend = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
            let backdrop = DWMSBT_TRANSIENTWINDOW;
            let backdrop_set = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &backdrop as *const _ as *const c_void,
                    std::mem::size_of::<i32>() as u32,
                )
            };

            if extend != 0 || backdrop_set != 0 {
                tracing::warn!(
                    extend,
                    backdrop_set,
                    "DWM acrylic backdrop unavailable, using accent blur"
                );
                accent(hwnd, ACCENT_ENABLE_ACRYLICBLURBEHIND, ACRYLIC_TINT);
            } else {
                tracing::info!("DWM acrylic backdrop enabled for the Windows 7 theme");
            }
        } else {
            let none = DWMSBT_NONE;
            unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &none as *const _ as *const c_void,
                    std::mem::size_of::<i32>() as u32,
                );
            }
            let margins = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: 0,
                cyBottomHeight: 0,
            };
            unsafe {
                DwmExtendFrameIntoClientArea(hwnd, &margins);
            }
            accent(hwnd, ACCENT_DISABLED, 0);
        }

        unsafe {
            InvalidateRect(hwnd, null(), 1);
        }
    }

    pub fn client_geometry(hwnd: isize) -> Option<super::ClientGeometry> {
        let hwnd = hwnd as HWND;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let mut origin = POINT { x: 0, y: 0 };
        unsafe {
            if GetClientRect(hwnd, &mut rect) == 0 || ClientToScreen(hwnd, &mut origin) == 0 {
                return None;
            }
        }
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if width == 0 || height == 0 {
            return None;
        }
        Some(super::ClientGeometry {
            x: origin.x,
            y: origin.y,
            width,
            height,
        })
    }

    fn accent(hwnd: HWND, state: u32, gradient_color: u32) {
        let Some(set_composition) = composition_api() else {
            tracing::warn!("SetWindowCompositionAttribute is unavailable; blur stays off");
            return;
        };
        let mut policy = AccentPolicy {
            state,
            flags: 0,
            gradient_color,
            animation_id: 0,
        };
        let mut data = WindowCompositionAttributeData {
            attribute: WCA_ACCENT_POLICY,
            data: &mut policy as *mut _ as *mut c_void,
            size_of_data: std::mem::size_of::<AccentPolicy>(),
        };
        unsafe {
            set_composition(hwnd, &mut data);
        }
    }

    /// The accent blur API is undocumented and therefore missing from
    /// `user32.lib`; it has to be resolved at runtime by name.
    fn composition_api() -> Option<SetWindowCompositionAttribute> {
        /// "user32" encoded as UTF-16 with the trailing zero.
        const USER32: [u16; 7] = [0x75, 0x73, 0x65, 0x72, 0x33, 0x32, 0x0000];

        let module = unsafe { GetModuleHandleW(USER32.as_ptr()) };
        if module.is_null() {
            return None;
        }
        let proc =
            unsafe { GetProcAddress(module, c"SetWindowCompositionAttribute".as_ptr().cast()) }?;
        Some(unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, SetWindowCompositionAttribute>(
                proc,
            )
        })
    }

    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> i32;
}
