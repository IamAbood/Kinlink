//! Screen capture via DXGI Desktop Duplication, with the host cursor composited into the frame.
use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;

#[derive(Clone, Debug)]
pub struct Monitor {
    pub name: String,
    /// Position/size in virtual-desktop pixels.
    pub rect: RECT,
    adapter: u32,
    output: u32,
}

pub fn monitors() -> Vec<Monitor> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(d) = output.GetDesc() {
                    if d.AttachedToDesktop.as_bool() {
                        let r = d.DesktopCoordinates;
                        out.push(Monitor {
                            name: format!("Display {} ({}x{})", out.len() + 1, r.right - r.left, r.bottom - r.top),
                            rect: r,
                            adapter: a,
                            output: o,
                        });
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
    out
}

#[derive(Default)]
struct Cursor {
    visible: bool,
    x: i32,
    y: i32,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    shape: Vec<u8>,
    version: u32,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Change {
    pub image: bool,
    pub cursor: bool,
}

pub struct CursorImage {
    pub w: u16,
    pub h: u16,
    pub hx: u16,
    pub hy: u16,
    pub rgba: Vec<u8>,
}

pub struct Capturer {
    pub monitor: Monitor,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    _output: IDXGIOutput1, // must outlive the duplication
    staging: Option<ID3D11Texture2D>,
    pub width: usize,
    pub height: usize,
    /// Last desktop image (BGRA, tightly packed).
    clean: Vec<u8>,
    cursor: Cursor,
}

// D3D11/DXGI objects used from one thread at a time (behind a Mutex in the helper).
unsafe impl Send for Capturer {}

impl Capturer {
    pub fn new(monitor: Monitor) -> Result<Self> {
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>()?;
            let adapter = factory.EnumAdapters1(monitor.adapter)?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(monitor.output)?.cast()?;
            let (mut device, mut ctx) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("no device")?;
            let dup = output.DuplicateOutput(&device).context("DuplicateOutput (another app may be using it, or this is a secure desktop)")?;
            let desc = dup.GetDesc();
            let (w, h) = (desc.ModeDesc.Width as usize, desc.ModeDesc.Height as usize);
            Ok(Self {
                monitor,
                device,
                ctx: ctx.context("no context")?,
                dup,
                _output: output,
                staging: None,
                width: w,
                height: h,
                clean: vec![0; w * h * 4],
                cursor: Cursor::default(),
            })
        }
    }

    /// Waits up to `timeout_ms` for a change. `out` receives the desktop image (no cursor: the
    /// client draws it) only when the image changed. Err = duplication lost; recreate the Capturer.
    pub fn next_frame(&mut self, timeout_ms: u32, out: &mut Vec<u8>) -> Result<Change> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Change::default()),
                Err(e) => bail!("capture lost: {e}"),
            }
            let result = self.process(&info, res);
            let _ = self.dup.ReleaseFrame();
            let ch = result?;
            if ch.image {
                out.clear();
                out.extend_from_slice(&self.clean);
            }
            Ok(ch)
        }
    }

    unsafe fn process(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, res: Option<IDXGIResource>) -> Result<Change> {
        let mut ch = Change::default();
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let now = (p.Visible.as_bool(), p.Position.x, p.Position.y);
            ch.cursor = now != (self.cursor.visible, self.cursor.x, self.cursor.y);
            (self.cursor.visible, self.cursor.x, self.cursor.y) = now;
        }
        if info.PointerShapeBufferSize > 0 {
            self.cursor.shape.resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0;
            if self.dup.GetFramePointerShape(info.PointerShapeBufferSize, self.cursor.shape.as_mut_ptr() as _, &mut needed, &mut self.cursor.info).is_ok() {
                self.cursor.version += 1;
                ch.cursor = true;
            }
        }
        if info.LastPresentTime != 0 {
            if let Some(res) = res {
                let tex: ID3D11Texture2D = res.cast()?;
                self.copy_image(&tex)?;
                ch.image = true;
            }
        }
        Ok(ch)
    }

    /// (visible, x, y in monitor pixels, shape version)
    pub fn cursor(&self) -> (bool, i32, i32, u32) {
        (self.cursor.visible, self.cursor.x, self.cursor.y, self.cursor.version)
    }

    /// Current cursor shape as straight RGBA. Pixels that would invert the screen (I-beam, etc.)
    /// become black with a white outline so they stay visible on any background.
    pub fn cursor_image(&self) -> Option<CursorImage> {
        let c = &self.cursor;
        if c.shape.is_empty() {
            return None;
        }
        let si = &c.info;
        let pitch = si.Pitch as usize;
        let mono = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let masked = si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
        let (w, h) = (si.Width as usize, if mono { si.Height as usize / 2 } else { si.Height as usize });
        if w == 0 || h == 0 || w > 96 || h > 96 {
            return None;
        }
        let mut rgba = vec![0u8; w * h * 4];
        let mut invert = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let px = &mut rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                if mono {
                    let bit = 0x80 >> (x % 8);
                    let and = c.shape.get(y * pitch + x / 8)? & bit != 0;
                    let xor = c.shape.get((y + h) * pitch + x / 8)? & bit != 0;
                    match (and, xor) {
                        (true, false) => {}
                        (false, false) => px.copy_from_slice(&[0, 0, 0, 255]),
                        (false, true) => px.copy_from_slice(&[255, 255, 255, 255]),
                        (true, true) => invert[y * w + x] = true,
                    }
                } else {
                    let s = y * pitch + x * 4;
                    let (b, g, r, a) = (*c.shape.get(s)?, c.shape[s + 1], c.shape[s + 2], c.shape[s + 3]);
                    if masked {
                        if a == 0 {
                            px.copy_from_slice(&[r, g, b, 255]);
                        } else if (r, g, b) != (0, 0, 0) {
                            invert[y * w + x] = true;
                        }
                    } else {
                        px.copy_from_slice(&[r, g, b, a]);
                    }
                }
            }
        }
        for y in 0..h {
            for x in 0..w {
                if invert[y * w + x] {
                    rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[0, 0, 0, 255]);
                    for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                        if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                            let i = ny as usize * w + nx as usize;
                            if !invert[i] && rgba[i * 4 + 3] == 0 {
                                rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                            }
                        }
                    }
                }
            }
        }
        Some(CursorImage { w: w as u16, h: h as u16, hx: si.HotSpot.x.max(0) as u16, hy: si.HotSpot.y.max(0) as u16, rgba })
    }

    unsafe fn copy_image(&mut self, tex: &ID3D11Texture2D) -> Result<()> {
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width as u32,
                Height: self.height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
            self.staging = t;
        }
        let staging = self.staging.as_ref().unwrap();
        self.ctx.CopyResource(staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let row = self.width * 4;
        for y in 0..self.height {
            let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), row);
            self.clean[y * row..(y + 1) * row].copy_from_slice(src);
        }
        self.ctx.Unmap(staging, 0);
        Ok(())
    }
}

// ---------------------------------------------------------------- one window

/// Windows a host can share: visible top-level app windows with a title (not ours).
pub fn windows() -> Vec<(isize, String)> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> windows::core::BOOL {
        unsafe {
            let out = &mut *(lp.0 as *mut Vec<(isize, String)>);
            let mut pid = 0;
            GetWindowThreadProcessId(h, Some(&mut pid));
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if IsWindowVisible(h).as_bool() && cloaked == 0 && pid != std::process::id() && GetWindow(h, GW_OWNER).is_err() && ex & WS_EX_TOOLWINDOW.0 == 0 {
                let title = window_title(h.0 as isize);
                if !title.is_empty() {
                    out.push((h.0 as isize, title));
                }
            }
            true.into()
        }
    }
    let mut out = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

pub fn window_title(hwnd: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(windows::Win32::Foundation::HWND(hwnd as _), &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Where a window is on screen (what its capture shows), or None if it's gone.
pub fn window_rect(hwnd: isize) -> Option<RECT> {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    let h = windows::Win32::Foundation::HWND(hwnd as _);
    unsafe {
        if !windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(h)).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, std::mem::size_of::<RECT>() as u32).ok()?;
        Some(r)
    }
}

/// Captures one window (Windows Graphics Capture): only that window, even when something
/// covers it. The cursor is drawn into the image.
pub struct WindowCapturer {
    pub hwnd: isize,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    d3d: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
    pool: windows::Graphics::Capture::Direct3D11CaptureFramePool,
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    pool_size: windows::Graphics::SizeInt32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Even-sized frame dimensions of the last image.
    pub width: usize,
    pub height: usize,
}

unsafe impl Send for WindowCapturer {}

impl WindowCapturer {
    pub fn new(hwnd: isize) -> Result<Self> {
        use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
        use windows::Graphics::DirectX::DirectXPixelFormat;
        use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
        use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            let (mut device, mut ctx) = (None, None);
            D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx)).context("D3D11CreateDevice")?;
            let device = device.context("no device")?;
            let d3d = CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)?.cast()?;
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(windows::Win32::Foundation::HWND(hwnd as _)).context("that window can't be captured")?;
            let size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsBorderRequired(false); // no yellow frame where Windows allows it
            session.StartCapture()?;
            Ok(Self { hwnd, device, ctx: ctx.context("no context")?, d3d, pool, _session: session, pool_size: size, staging: None, width: 0, height: 0 })
        }
    }

    /// Waits up to `timeout_ms` for a new image; true = `out` got it (BGRA, width x height).
    // ponytail: polls the free-threaded pool every 2 ms; use the FrameArrived event if this shows up in CPU use
    pub fn next_frame(&mut self, timeout_ms: u32, out: &mut Vec<u8>) -> Result<bool> {
        use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
        let until = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        let frame = loop {
            if let Ok(f) = self.pool.TryGetNextFrame() {
                break f;
            }
            if std::time::Instant::now() >= until {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let size = frame.ContentSize()?;
        if size.Width <= 1 || size.Height <= 1 {
            return Ok(false); // minimized
        }
        unsafe {
            let tex: ID3D11Texture2D = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            if self.staging.as_ref().is_none_or(|s| (s.1, s.2) != (desc.Width, desc.Height)) {
                let sd = D3D11_TEXTURE2D_DESC { Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0, MipLevels: 1, ArraySize: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, ..desc };
                let mut t = None;
                self.device.CreateTexture2D(&sd, None, Some(&mut t))?;
                self.staging = Some((t.context("no texture")?, desc.Width, desc.Height));
            }
            let staging = &self.staging.as_ref().unwrap().0;
            self.ctx.CopyResource(staging, &tex);
            let (w, h) = ((size.Width as u32).min(desc.Width) as usize & !1, (size.Height as u32).min(desc.Height) as usize & !1);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            out.resize(w * h * 4, 0);
            for y in 0..h {
                let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w * 4);
                out[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
            }
            self.ctx.Unmap(staging, 0);
            (self.width, self.height) = (w, h);
        }
        drop(frame);
        // The window was resized: the pool must match or the image gets cut off.
        if (size.Width, size.Height) != (self.pool_size.Width, self.pool_size.Height) {
            self.pool.Recreate(&self.d3d, windows::Graphics::DirectX::DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
            self.pool_size = size;
        }
        Ok(true)
    }
}
