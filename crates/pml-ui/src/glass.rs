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

struct Snapshot {
    pixels: Vec<u8>,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
}

impl Backdrop {
    /// Grabs the screen right now and starts blurring it in the background.
    /// Call this before creating the launcher window.
    pub fn capture_now() -> Self {
        let mut backdrop = Self {
            rx: None,
            texture: None,
            origin_x: 0,
            origin_y: 0,
            width: 0,
            height: 0,
        };

        #[cfg(windows)]
        {
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
        }

        backdrop
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

    #[repr(C)]
    struct BitmapInfoHeader {
        size: u32,
        width: i32,
        height: i32,
        planes: u16,
        bit_count: u16,
        compression: u32,
        size_image: u32,
        x_pels_per_meter: i32,
        y_pels_per_meter: i32,
        colors_used: u32,
        colors_important: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        header: BitmapInfoHeader,
        colors: [u32; 1],
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn CreateCompatibleBitmap(hdc: isize, width: i32, height: i32) -> isize;
        fn SelectObject(hdc: isize, object: isize) -> isize;
        fn BitBlt(
            dst: isize,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            src: isize,
            src_x: i32,
            src_y: i32,
            rop: u32,
        ) -> i32;
        fn GetDIBits(
            hdc: isize,
            bitmap: isize,
            start: u32,
            lines: u32,
            bits: *mut c_void,
            info: *mut BitmapInfo,
            usage: u32,
        ) -> i32;
        fn DeleteObject(object: isize) -> i32;
        fn DeleteDC(hdc: isize) -> i32;
    }

    const SRCCOPY: u32 = 0x00CC_0020;
    /// Include layered windows (Wallpaper Engine and friends).
    const CAPTUREBLT: u32 = 0x4000_0000;
    const DIB_RGB_COLORS: u32 = 0;

    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const SM_XVIRTUALSCREEN: i32 = 76;
    const SM_YVIRTUALSCREEN: i32 = 77;
    const SM_CXVIRTUALSCREEN: i32 = 78;
    const SM_CYVIRTUALSCREEN: i32 = 79;

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

    let screen_dc = unsafe { GetDC(0) };
    if screen_dc == 0 {
        return None;
    }
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc == 0 {
        unsafe {
            ReleaseDC(0, screen_dc);
        }
        return None;
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
    if bitmap == 0 {
        unsafe {
            DeleteDC(memory_dc);
            ReleaseDC(0, screen_dc);
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
    let mut info = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width,
            // Negative height requests a top-down buffer.
            height: -height,
            planes: 1,
            bit_count: 32,
            compression: 0, // BI_RGB
            size_image: 0,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            colors_used: 0,
            colors_important: 0,
        },
        colors: [0],
    };
    let scanned_lines = if copied != 0 {
        unsafe {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr() as *mut c_void,
                &mut info,
                DIB_RGB_COLORS,
            )
        }
    } else {
        0
    };

    unsafe {
        if old_bitmap != 0 {
            SelectObject(memory_dc, old_bitmap);
        }
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(0, screen_dc);
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

    #[repr(C)]
    #[derive(Default)]
    struct Margins {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

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

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmExtendFrameIntoClientArea(hwnd: isize, margins: *const Margins) -> i32;
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const c_void,
            size: u32,
        ) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn InvalidateRect(hwnd: isize, rect: *const c_void, erase: i32) -> i32;
        fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn ClientToScreen(hwnd: isize, point: *mut Point) -> i32;
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
        if enabled {
            let margins = Margins {
                left: -1,
                top: -1,
                right: -1,
                bottom: -1,
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
            let margins = Margins::default();
            unsafe {
                DwmExtendFrameIntoClientArea(hwnd, &margins);
            }
            accent(hwnd, ACCENT_DISABLED, 0);
        }

        unsafe {
            InvalidateRect(hwnd, std::ptr::null(), 1);
        }
    }

    pub fn client_geometry(hwnd: isize) -> Option<super::ClientGeometry> {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let mut origin = Point { x: 0, y: 0 };
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

    fn accent(hwnd: isize, state: u32, gradient_color: u32) {
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
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleW(name: *const u16) -> isize;
            fn GetProcAddress(module: isize, name: *const u8) -> *const c_void;
        }

        /// "user32" encoded as UTF-16 with the trailing zero.
        const USER32: [u16; 7] = [0x75, 0x73, 0x65, 0x72, 0x33, 0x32, 0x0000];

        let module = unsafe { GetModuleHandleW(USER32.as_ptr()) };
        if module == 0 {
            return None;
        }
        let proc =
            unsafe { GetProcAddress(module, c"SetWindowCompositionAttribute".as_ptr().cast()) };
        if proc.is_null() {
            return None;
        }
        Some(unsafe { std::mem::transmute::<*const c_void, SetWindowCompositionAttribute>(proc) })
    }

    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(isize, *mut WindowCompositionAttributeData) -> i32;
}
