use std::{collections::HashMap, ffi::c_void, sync::LazyLock};

use parking_lot::Mutex;
use tauri::WebviewWindow;
use windows::{
    core::{Interface, Type},
    Win32::{
        Foundation::{HMODULE, HWND, POINT, RECT, SIZE},
        Graphics::{
            Direct2D::Common::{D2D1_BORDER_MODE_HARD, D2D1_COLOR_F},
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11Resource, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_SDK_VERSION,
            },
            DirectComposition::{
                DCompositionCreateDevice3, IDCompositionDesktopDevice, IDCompositionDevice3,
                IDCompositionFilterEffect, IDCompositionGaussianBlurEffect,
                IDCompositionSaturationEffect, IDCompositionScaleTransform, IDCompositionSurface,
                IDCompositionTarget, IDCompositionTranslateTransform, IDCompositionVisual,
                IDCompositionVisual2,
            },
            Dwm::DwmUnregisterThumbnail,
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM},
                IDXGIDevice, IDXGISurface,
            },
            Gdi::ClientToScreen,
        },
        System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
        UI::WindowsAndMessaging::{
            GetClientRect, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

const BLUR_RADIUS: f32 = 34.0;

#[derive(Debug)]
struct HostBackdropError(String);

impl From<windows::core::Error> for HostBackdropError {
    fn from(error: windows::core::Error) -> Self {
        Self(error.to_string())
    }
}

type HostResult<T> = Result<T, HostBackdropError>;

type CreateSharedMultiWindowVisual = unsafe extern "system" fn(
    HWND,
    *mut c_void,
    *mut *mut c_void,
    *mut isize,
) -> windows::core::HRESULT;
type UpdateSharedMultiWindowVisual = unsafe extern "system" fn(
    isize,
    *const HWND,
    u32,
    *const HWND,
    u32,
    *mut RECT,
    *mut SIZE,
    u32,
) -> windows::core::HRESULT;

// Global rather than thread-local: the first apply may run on an async command
// worker thread while later updates and the minimize guard run on the main
// thread. All DComp/D3D access is serialized by this lock.
static INSTANCES: LazyLock<Mutex<HashMap<isize, HostBackdrop>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Set once construction has failed: the DWM shared-visual API is
/// build-dependent (every ordinal-164 update variant returns E_INVALIDARG on
/// some insider builds), so retrying on every theme apply would repeatedly
/// create and destroy a DComp target on the window — churn that can leave the
/// SWCA fallback material stuck until DWM rebuilds the window on a resize.
static UNSUPPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct HostBackdrop {
    d3d: ID3D11Device,
    device: IDCompositionDesktopDevice,
    _device3: IDCompositionDevice3,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual2,
    _backdrop_root: IDCompositionVisual2,
    _host_visual: IDCompositionVisual2,
    _backing_visual: IDCompositionVisual2,
    _tint_visual: IDCompositionVisual2,
    backing_surface: IDCompositionSurface,
    tint_surface: IDCompositionSurface,
    backing_scale: IDCompositionScaleTransform,
    tint_scale: IDCompositionScaleTransform,
    host_translate: IDCompositionTranslateTransform,
    _blur: IDCompositionGaussianBlurEffect,
    _saturation: IDCompositionSaturationEffect,
    thumbnail: isize,
    update_shared: UpdateSharedMultiWindowVisual,
}

// SAFETY: instances are only reached through the INSTANCES mutex, so all COM
// access is serialized. The D3D11 device is free-threaded, and DirectComposition
// / DWM thumbnail calls only require that the caller serializes access.
unsafe impl Send for HostBackdrop {}

/// Owns a DWM shared-visual registration until construction hands it to
/// `HostBackdrop`; unregisters it when a later construction step fails.
struct ThumbnailGuard(isize);

impl ThumbnailGuard {
    fn release(mut self) -> isize {
        std::mem::replace(&mut self.0, 0)
    }
}

impl Drop for ThumbnailGuard {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = DwmUnregisterThumbnail(self.0);
            }
        }
    }
}

impl Drop for HostBackdrop {
    fn drop(&mut self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.thumbnail);
        }
    }
}

pub fn apply(window: &WebviewWindow, dark: bool) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let hwnd = HWND(window.hwnd().map_err(|error| error.to_string())?.0 as *mut c_void);
    let mut instances = INSTANCES.lock();
    if let Some(instance) = instances.get_mut(&(hwnd.0 as isize)) {
        instance
            .update(hwnd, dark)
            .map_err(|error| format!("update shared visual coordinates: {error}"))?;
        return Ok(());
    }
    if UNSUPPORTED.load(Ordering::Relaxed) {
        return Err("host backdrop unsupported on this OS build (cached)".into());
    }
    let instance = HostBackdrop::new(hwnd, dark).map_err(|error| {
        UNSUPPORTED.store(true, Ordering::Relaxed);
        error.0
    })?;
    instances.insert(hwnd.0 as isize, instance);
    Ok(())
}

/// Cover the underlay with an opaque tint and wait until DWM has composed it.
/// Returns false when no backdrop instance exists for the window.
pub fn set_solid(hwnd_key: isize, dark: bool) -> bool {
    let mut instances = INSTANCES.lock();
    let Some(instance) = instances.get_mut(&hwnd_key) else {
        return false;
    };
    instance.paint_cover(dark).is_ok()
}

/// Restore the live translucent tint and refresh the shared visual coordinates.
/// Returns false when no backdrop instance exists for the window.
pub fn set_live(hwnd_key: isize, dark: bool) -> bool {
    let mut instances = INSTANCES.lock();
    let Some(instance) = instances.get_mut(&hwnd_key) else {
        return false;
    };
    instance.update(HWND(hwnd_key as *mut c_void), dark).is_ok()
}

/// Whether host-backdrop construction has already failed on this OS build.
pub fn unsupported() -> bool {
    UNSUPPORTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Drop the backdrop instance for a destroyed window. Returns true when an
/// instance existed.
pub fn remove(hwnd_key: isize) -> bool {
    INSTANCES.lock().remove(&hwnd_key).is_some()
}

impl HostBackdrop {
    fn new(hwnd: HWND, dark: bool) -> HostResult<Self> {
        let step =
            |name: &str, error: windows::core::Error| HostBackdropError(format!("{name}: {error}"));
        let d3d = create_d3d_device().map_err(|error| step("create D3D11 device", error))?;
        let dxgi: IDXGIDevice = d3d
            .cast()
            .map_err(|error| step("query IDXGIDevice", error))?;
        let device: IDCompositionDesktopDevice = unsafe { DCompositionCreateDevice3(&dxgi) }
            .map_err(|error| step("create IDCompositionDesktopDevice", error))?;
        let device3: IDCompositionDevice3 = device
            .cast()
            .map_err(|error| step("query IDCompositionDevice3", error))?;

        let target = unsafe { device.CreateTargetForHwnd(hwnd, false) }
            .map_err(|error| step("create HWND composition target", error))?;
        let root =
            unsafe { device3.CreateVisual() }.map_err(|error| step("create root visual", error))?;
        let backing_visual = unsafe { device3.CreateVisual() }
            .map_err(|error| step("create backing visual", error))?;
        let backdrop_root = unsafe { device3.CreateVisual() }
            .map_err(|error| step("create backdrop visual", error))?;
        let tint_visual =
            unsafe { device3.CreateVisual() }.map_err(|error| step("create tint visual", error))?;

        let backing_surface = unsafe {
            device3
                .CreateSurface(
                    1,
                    1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .map_err(|error| step("create backing surface", error))?
        };
        let tint_surface = unsafe {
            device3
                .CreateSurface(
                    1,
                    1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .map_err(|error| step("create tint surface", error))?
        };
        paint_surface(&d3d, &backing_surface, backing_color(dark))
            .map_err(|error| step("paint backing surface", error))?;
        paint_surface(&d3d, &tint_surface, tint_color(dark))
            .map_err(|error| step("paint tint surface", error))?;
        unsafe {
            backing_visual
                .SetContent(&backing_surface)
                .map_err(|error| step("set backing content", error))?;
            tint_visual
                .SetContent(&tint_surface)
                .map_err(|error| step("set tint content", error))?;
        }

        let backing_scale = unsafe { device3.CreateScaleTransform() }
            .map_err(|error| step("create backing scale", error))?;
        let tint_scale = unsafe { device3.CreateScaleTransform() }
            .map_err(|error| step("create tint scale", error))?;
        let host_translate = unsafe { device3.CreateTranslateTransform() }
            .map_err(|error| step("create backdrop translation", error))?;
        unsafe {
            backing_visual
                .SetTransform(&backing_scale)
                .map_err(|error| step("bind backing transform", error))?;
            tint_visual
                .SetTransform(&tint_scale)
                .map_err(|error| step("bind tint transform", error))?;
            backdrop_root
                .SetTransform(&host_translate)
                .map_err(|error| step("bind backdrop transform", error))?;
        }

        let (create_shared, update_shared) = resolve_shared_visual_functions()
            .map_err(|error| step("resolve DWM shared visual exports", error))?;
        let mut host_raw = std::ptr::null_mut();
        let mut thumbnail = 0;
        unsafe {
            create_shared(
                hwnd,
                Interface::as_raw(&device),
                &mut host_raw,
                &mut thumbnail,
            )
            .ok()
            .map_err(|error| step("create DWM shared multi-window visual", error))?;
        }
        // The DWM-side registration must not leak if a later step fails.
        let thumbnail = ThumbnailGuard(thumbnail);
        let host_visual = unsafe { IDCompositionVisual2::from_abi(host_raw) }
            .map_err(|error| step("adopt DWM shared visual as IDCompositionVisual2", error))?;

        let saturation = unsafe { device3.CreateSaturationEffect() }
            .map_err(|error| step("create saturation effect", error))?;
        unsafe {
            // Input 0 is deliberately left unbound: a filter effect input only
            // accepts other filter effects, and an unbound input receives the
            // rasterized subtree of the visual the effect is attached to (the
            // shared desktop visual below).
            saturation
                .SetSaturation2(1.10)
                .map_err(|error| step("configure saturation", error))?;
        }
        let blur = unsafe { device3.CreateGaussianBlurEffect() }
            .map_err(|error| step("create Gaussian blur effect", error))?;
        let blur_filter: IDCompositionFilterEffect = blur
            .cast()
            .map_err(|error| step("query Gaussian blur filter", error))?;
        unsafe {
            blur.SetStandardDeviation2(BLUR_RADIUS)
                .map_err(|error| step("configure blur radius", error))?;
            blur.SetBorderMode(D2D1_BORDER_MODE_HARD)
                .map_err(|error| step("configure blur border", error))?;
            blur_filter
                .SetInput(0, &saturation, 0)
                .map_err(|error| step("chain saturation into blur", error))?;
            backdrop_root
                .SetEffect(&blur)
                .map_err(|error| step("attach blur effect", error))?;

            backdrop_root
                .AddVisual(&host_visual, false, None::<&IDCompositionVisual>)
                .map_err(|error| step("add shared visual", error))?;
            root.AddVisual(&backing_visual, false, None::<&IDCompositionVisual>)
                .map_err(|error| step("add backing visual", error))?;
            root.AddVisual(&backdrop_root, true, &backing_visual)
                .map_err(|error| step("add backdrop visual", error))?;
            root.AddVisual(&tint_visual, true, &backdrop_root)
                .map_err(|error| step("add tint visual", error))?;
            target
                .SetRoot(&root)
                .map_err(|error| step("attach composition root", error))?;
        }

        let mut result = Self {
            d3d,
            device,
            _device3: device3,
            _target: target,
            _root: root,
            _backdrop_root: backdrop_root,
            _host_visual: host_visual,
            _backing_visual: backing_visual,
            _tint_visual: tint_visual,
            backing_surface,
            tint_surface,
            backing_scale,
            tint_scale,
            host_translate,
            _blur: blur,
            _saturation: saturation,
            thumbnail: thumbnail.release(),
            update_shared,
        };
        result
            .update(hwnd, dark)
            .map_err(|error| step("initialize shared visual coordinates", error))?;
        Ok(result)
    }

    fn update(&mut self, hwnd: HWND, dark: bool) -> windows::core::Result<()> {
        paint_surface(&self.d3d, &self.backing_surface, backing_color(dark))?;
        paint_surface(&self.d3d, &self.tint_surface, tint_color(dark))?;

        let mut client = RECT::default();
        let mut origin = POINT::default();
        unsafe {
            GetClientRect(hwnd, &mut client)?;
            ClientToScreen(hwnd, &mut origin).ok()?;
            self.backing_scale
                .SetScaleX2((client.right - client.left).max(1) as f32)?;
            self.backing_scale
                .SetScaleY2((client.bottom - client.top).max(1) as f32)?;
            self.tint_scale
                .SetScaleX2((client.right - client.left).max(1) as f32)?;
            self.tint_scale
                .SetScaleY2((client.bottom - client.top).max(1) as f32)?;
            self.host_translate.SetOffsetX2(-(origin.x as f32))?;
            self.host_translate.SetOffsetY2(-(origin.y as f32))?;
        }

        let mut source = RECT {
            left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            right: 0,
            bottom: 0,
        };
        source.right = source.left + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        source.bottom = source.top + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        let mut destination = SIZE {
            cx: source.right - source.left,
            cy: source.bottom - source.top,
        };
        let exclude = [hwnd];
        unsafe {
            (self.update_shared)(
                self.thumbnail,
                std::ptr::null(),
                0,
                exclude.as_ptr(),
                exclude.len() as u32,
                &mut source,
                &mut destination,
                1,
            )
            .ok()?;
            self.device.Commit()?;
        }
        Ok(())
    }

    /// Paint the topmost tint layer fully opaque so the material never shows the
    /// desktop through it while DWM plays a window transition. Blocks until DWM
    /// has composed the change so the transition cannot start on a stale frame.
    fn paint_cover(&mut self, dark: bool) -> windows::core::Result<()> {
        paint_surface(&self.d3d, &self.tint_surface, backing_color(dark))?;
        unsafe {
            self.device.Commit()?;
            self.device.WaitForCommitCompletion()?;
        }
        Ok(())
    }
}

fn create_d3d_device() -> windows::core::Result<ID3D11Device> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        if unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .is_ok()
        {
            if let Some(device) = device {
                return Ok(device);
            }
        }
    }
    Err(windows::core::Error::from_win32())
}

fn paint_surface(
    device: &ID3D11Device,
    surface: &IDCompositionSurface,
    color: D2D1_COLOR_F,
) -> windows::core::Result<()> {
    let mut offset = POINT::default();
    let dxgi_surface: IDXGISurface = unsafe { surface.BeginDraw(None, &mut offset)? };
    let resource: ID3D11Resource = dxgi_surface.cast()?;
    let mut target = None;
    unsafe {
        device.CreateRenderTargetView(&resource, None, Some(&mut target))?;
        let context = device.GetImmediateContext()?;
        context.ClearRenderTargetView(
            &target.expect("CreateRenderTargetView succeeded without a target"),
            &[color.r, color.g, color.b, color.a],
        );
        surface.EndDraw()?;
    }
    Ok(())
}

fn resolve_shared_visual_functions(
) -> windows::core::Result<(CreateSharedMultiWindowVisual, UpdateSharedMultiWindowVisual)> {
    let module = unsafe { GetModuleHandleW(windows::core::w!("dwmapi.dll"))? };
    let create = resolve_ordinal(module, 163).ok_or_else(windows::core::Error::from_win32)?;
    let update = resolve_ordinal(module, 164).ok_or_else(windows::core::Error::from_win32)?;
    Ok(unsafe {
        (
            std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                CreateSharedMultiWindowVisual,
            >(create),
            std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                UpdateSharedMultiWindowVisual,
            >(update),
        )
    })
}

fn resolve_ordinal(module: HMODULE, ordinal: u16) -> windows::Win32::Foundation::FARPROC {
    unsafe { GetProcAddress(module, windows::core::PCSTR(ordinal as usize as *const u8)) }
}

fn backing_color(dark: bool) -> D2D1_COLOR_F {
    if dark {
        D2D1_COLOR_F {
            r: 18.0 / 255.0,
            g: 18.0 / 255.0,
            b: 22.0 / 255.0,
            a: 1.0,
        }
    } else {
        D2D1_COLOR_F {
            r: 243.0 / 255.0,
            g: 243.0 / 255.0,
            b: 245.0 / 255.0,
            a: 1.0,
        }
    }
}

fn tint_color(dark: bool) -> D2D1_COLOR_F {
    if dark {
        D2D1_COLOR_F {
            r: 18.0 / 255.0,
            g: 18.0 / 255.0,
            b: 22.0 / 255.0,
            a: 0.78,
        }
    } else {
        D2D1_COLOR_F {
            r: 243.0 / 255.0,
            g: 243.0 / 255.0,
            b: 245.0 / 255.0,
            a: 0.82,
        }
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetShellWindow, RegisterClassW, ShowWindow,
        SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };

    type UpdateShared9 = unsafe extern "system" fn(
        isize,
        *const HWND,
        u32,
        *const HWND,
        u32,
        *mut RECT,
        *mut SIZE,
        u32,
        usize,
    ) -> windows::core::HRESULT;

    unsafe extern "system" fn probe_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn virtual_source() -> (RECT, SIZE) {
        let mut source = RECT {
            left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            right: 0,
            bottom: 0,
        };
        source.right = source.left + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        source.bottom = source.top + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        let destination = SIZE {
            cx: source.right - source.left,
            cy: source.bottom - source.top,
        };
        (source, destination)
    }

    fn primary_source() -> (RECT, SIZE) {
        let source = RECT {
            left: 0,
            top: 0,
            right: unsafe { GetSystemMetrics(SM_CXSCREEN) },
            bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) },
        };
        let destination = SIZE {
            cx: source.right,
            cy: source.bottom,
        };
        (source, destination)
    }

    #[test]
    #[ignore = "manual probe for the undocumented DWM shared-visual update API"]
    fn probe_update_shared_variants() {
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let class_name = windows::core::w!("gmplayer_dwm_probe");
            let class = WNDCLASSW {
                lpfnWndProc: Some(probe_proc),
                hInstance: module.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name,
                windows::core::w!("probe"),
                WS_POPUP,
                80,
                80,
                420,
                320,
                None,
                None,
                Some(module.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

            let d3d = create_d3d_device().unwrap();
            let dxgi: IDXGIDevice = d3d.cast().unwrap();
            let device: IDCompositionDesktopDevice = DCompositionCreateDevice3(&dxgi).unwrap();
            let device3: IDCompositionDevice3 = device.cast().unwrap();
            let target = device.CreateTargetForHwnd(hwnd, false).unwrap();
            let root = device3.CreateVisual().unwrap();
            target.SetRoot(&root).unwrap();

            let (create_shared, update_shared) = resolve_shared_visual_functions().unwrap();
            let dwmapi = GetModuleHandleW(windows::core::w!("dwmapi.dll")).unwrap();
            let update_shared9 = std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                UpdateShared9,
            >(resolve_ordinal(dwmapi, 164).unwrap());

            let mut host_raw = std::ptr::null_mut();
            let mut thumbnail = 0isize;
            let hr = create_shared(hwnd, Interface::as_raw(&device), &mut host_raw, &mut thumbnail);
            println!(
                "create_shared: {:#010x} raw={:?} handle={:#x}",
                hr.0, host_raw, thumbnail
            );
            assert!(hr.is_ok(), "create_shared failed");

            let host_visual = IDCompositionVisual2::from_abi(host_raw).unwrap();
            root.AddVisual(&host_visual, false, None::<&IDCompositionVisual>)
                .unwrap();

            // Production order first: update before any commit.
            let exclude = [hwnd];
            let (mut source, mut destination) = virtual_source();
            let hr = update_shared(
                thumbnail,
                std::ptr::null(),
                0,
                exclude.as_ptr(),
                1,
                &mut source,
                &mut destination,
                1,
            );
            println!("pre-commit  excl-only flags=1        : {:#010x}", hr.0);

            device.Commit().unwrap();
            device.WaitForCommitCompletion().unwrap();

            let shell = GetShellWindow();
            let include = [shell];

            let variants: &[(&str, *const HWND, u32, *const HWND, u32, u32)] = &[
                ("excl-only flags=1", std::ptr::null(), 0, exclude.as_ptr(), 1, 1),
                ("excl-only flags=0", std::ptr::null(), 0, exclude.as_ptr(), 1, 0),
                ("excl-only flags=2", std::ptr::null(), 0, exclude.as_ptr(), 1, 2),
                ("excl-only flags=4", std::ptr::null(), 0, exclude.as_ptr(), 1, 4),
                ("empty lists flags=1", std::ptr::null(), 0, std::ptr::null(), 0, 1),
                ("incl shell flags=1", include.as_ptr(), 1, std::ptr::null(), 0, 1),
                (
                    "incl shell + excl flags=1",
                    include.as_ptr(),
                    1,
                    exclude.as_ptr(),
                    1,
                    1,
                ),
            ];
            for (name, inc, n_inc, exc, n_exc, flags) in variants {
                let (mut source, mut destination) = virtual_source();
                let hr = update_shared(
                    thumbnail,
                    *inc,
                    *n_inc,
                    *exc,
                    *n_exc,
                    &mut source,
                    &mut destination,
                    *flags,
                );
                println!("post-commit {name:28}: {:#010x}", hr.0);
            }

            let (mut source, mut destination) = primary_source();
            let hr = update_shared(
                thumbnail,
                std::ptr::null(),
                0,
                exclude.as_ptr(),
                1,
                &mut source,
                &mut destination,
                1,
            );
            println!("post-commit excl-only primary-rect   : {:#010x}", hr.0);

            let (mut source, mut destination) = virtual_source();
            let hr = update_shared9(
                thumbnail,
                std::ptr::null(),
                0,
                exclude.as_ptr(),
                1,
                &mut source,
                &mut destination,
                1,
                0,
            );
            println!("post-commit 9-arg trailing-0         : {:#010x}", hr.0);

            let _ = DwmUnregisterThumbnail(thumbnail);
            let _ = DestroyWindow(hwnd);
        }
    }
}
