#[cfg(any(target_os = "windows", target_os = "macos"))]
use tauri::window::Effect;
#[cfg(target_os = "macos")]
use tauri::window::EffectState;
use tauri::window::{Color, EffectsBuilder};
use tauri::{Manager, WebviewWindow};
#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::{BOOL, HWND},
    Graphics::Dwm::{DwmFlush, DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
    System::LibraryLoader::{GetModuleHandleA, GetProcAddress},
};
#[cfg(target_os = "windows")]
use windows_version::OsVersion;

use super::config::WindowConfig;

#[cfg(target_os = "windows")]
const WINDOWS_11_MIN_BUILD: u32 = 22_000;
#[cfg(target_os = "windows")]
const WCA_ACCENT_POLICY: u32 = 19;
#[cfg(target_os = "windows")]
const ACCENT_DISABLED: u32 = 0;
#[cfg(target_os = "windows")]
const ACCENT_ENABLE_GRADIENT: u32 = 1;
#[cfg(target_os = "windows")]
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;

/// Option 1 prototype toggle. When true, the Windows 11 main-window shell uses
/// the native acrylic system backdrop (`DWMSBT_TRANSIENTWINDOW`), which DWM
/// composes into peek/thumbnail/minimize surrogates just like any first-class
/// window. When false, it uses the custom DComp/SWCA "Host Mica Alt" material
/// (richer, self-tuned tint, but transparent during peek). Flip this one const —
/// nothing is deleted — to compare the two live.
#[cfg(target_os = "windows")]
const USE_NATIVE_BACKDROP: bool = true;

pub fn build_window_effects(
    effect: &str,
    color: Option<Color>,
) -> Option<tauri::utils::config::WindowEffectsConfig> {
    match effect {
        "system-shell" => build_system_shell_fallback(false),
        "acrylic" => acrylic_effects(color),
        _ => None,
    }
}

fn build_system_shell_fallback(dark: bool) -> Option<tauri::utils::config::WindowEffectsConfig> {
    #[cfg(target_os = "windows")]
    {
        let build = OsVersion::current().build;
        let effect = if build >= WINDOWS_11_MIN_BUILD {
            if dark {
                Effect::TabbedDark
            } else {
                Effect::TabbedLight
            }
        } else {
            Effect::Acrylic
        };
        let mut builder = EffectsBuilder::new().effect(effect);
        if build < WINDOWS_11_MIN_BUILD {
            builder = builder.color(system_shell_tint(dark));
        }
        return Some(builder.build());
    }

    #[cfg(target_os = "macos")]
    {
        let _ = dark;
        return Some(
            EffectsBuilder::new()
                .effect(Effect::Sidebar)
                .state(EffectState::FollowsWindowActiveState)
                .radius(12.0)
                .build(),
        );
    }

    #[allow(unreachable_code)]
    None
}

/// Apply the persistent system backdrop used while DWM has the custom Accent surface cloaked.
pub fn apply_system_shell_fallback(window: &WebviewWindow, dark: bool) -> Result<(), String> {
    if let Some(effects) = build_system_shell_fallback(dark) {
        window
            .set_effects(effects)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Apply a live under-window blur with a stronger Mica Alt-like tint on Windows 11.
pub fn apply_system_shell_effect(window: &WebviewWindow, dark: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    if OsVersion::current().build >= WINDOWS_11_MIN_BUILD {
        // Option 1 prototype: prefer the native acrylic system backdrop. DWM owns
        // it, so — unlike the custom DComp/SWCA material — it renders in the
        // taskbar thumbnail and Aero Peek preview instead of going transparent.
        if USE_NATIVE_BACKDROP {
            // The native backdrop handles minimize/restore itself; drop the guard.
            super::minimize_guard::uninstall(window);
            match apply_windows_native_backdrop(window, dark) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::warn!(
                        "Native acrylic backdrop unavailable, falling back to Mica Alt: {error}"
                    );
                    return apply_system_shell_fallback(window, dark);
                }
            }
        }
        match apply_windows_host_mica_alt(window, dark) {
            Ok(()) => return Ok(()),
            Err(error) => {
                log::warn!("Host Mica Alt unavailable, falling back to Mica Alt: {error}");
                // The system backdrop handles its own minimize fallback; drop the guard.
                super::minimize_guard::uninstall(window);
                return apply_system_shell_fallback(window, dark);
            }
        }
    }

    apply_system_shell_fallback(window, dark)
}

/// Apply the native acrylic system backdrop (`DWMSBT_TRANSIENTWINDOW`).
///
/// DWM composes this material as first-class window content, so it appears in the
/// taskbar thumbnail and the Aero Peek live preview the same as any window — the
/// custom DComp underlay / SWCA accent do not, which is why those surrogates show
/// the bare transparent webview. Native acrylic is a live under-window blur (it
/// samples what is behind the window), matching the previous look's behaviour; the
/// heavier app tint is layered back in from the frontend. The trade-off is that
/// the blur/tint are the system's, without the per-pixel radius/saturation control
/// of the DComp path.
#[cfg(target_os = "windows")]
fn apply_windows_native_backdrop(window: &WebviewWindow, dark: bool) -> Result<(), String> {
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMSBT_TRANSIENTWINDOW: u32 = 3;

    let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as HWND;

    // Repaint the non-client frame for the theme before the backdrop swaps in.
    let dark_mode: BOOL = dark as BOOL;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark_mode as *const _ as _,
            std::mem::size_of_val(&dark_mode) as u32,
        );
    }

    let backdrop: u32 = DWMSBT_TRANSIENTWINDOW;
    let hr = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as _,
            std::mem::size_of_val(&backdrop) as u32,
        )
    };
    if hr < 0 {
        return Err(format!(
            "DWMWA_SYSTEMBACKDROP_TYPE (acrylic) failed: {hr:#010x}"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    size: usize,
}

#[cfg(target_os = "windows")]
type SetWindowCompositionAttribute =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> BOOL;

/// Last theme successfully applied to each window's live SWCA material. Theme
/// switches can arrive more than once per toggle (duplicate frontend watchers)
/// and each redundant accent re-apply makes DWM rebuild the blur, which shows
/// as a visible flash — dedupe them here. Forced re-asserts (reveal, restore
/// from the minimize cover) bypass this by calling the `_hwnd` function
/// directly, which refreshes the cache.
#[cfg(target_os = "windows")]
static LAST_SWCA_THEME: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<isize, bool>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

#[cfg(target_os = "windows")]
fn apply_windows_host_mica_alt(window: &WebviewWindow, dark: bool) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as HWND;
    // While minimized the guard's opaque cover owns the surface: record the new
    // theme on the cover instead; the live material returns on restore.
    if super::minimize_guard::refresh_solid_if_minimized(hwnd as isize, dark) {
        return Ok(());
    }
    if super::host_backdrop::unsupported() {
        // The DComp host path already failed on this build; go straight to
        // SWCA without the construction attempt or the per-apply warn spam.
        if LAST_SWCA_THEME.lock().get(&(hwnd as isize)) == Some(&dark) {
            super::minimize_guard::install(window, dark);
            return Ok(());
        }
        apply_windows_host_mica_alt_hwnd(hwnd, dark)?;
    } else if let Err(error) = super::host_backdrop::apply(window, dark) {
        log::warn!("DirectComposition Host Mica Alt unavailable; using SWCA: {error}");
        apply_windows_host_mica_alt_hwnd(hwnd, dark)?;
    }
    super::minimize_guard::install(window, dark);
    Ok(())
}

/// Forget the cached SWCA theme for a destroyed window.
#[cfg(target_os = "windows")]
pub(super) fn forget_swca_theme(hwnd_key: isize) {
    LAST_SWCA_THEME.lock().remove(&hwnd_key);
}

/// Re-apply the live translucent material after the minimize guard's solid cover.
#[cfg(target_os = "windows")]
pub(super) fn apply_host_material_live(hwnd_key: isize, dark: bool) {
    if super::host_backdrop::set_live(hwnd_key, dark) {
        // The DComp underlay owns the visual again; retire the accent cover.
        disable_windows_live_backdrop(hwnd_key as HWND);
    } else if let Err(error) = apply_windows_host_mica_alt_hwnd(hwnd_key as HWND, dark) {
        log::warn!("Failed to restore the live window material: {error}");
    }
}

/// Swap the material for an opaque cover that DWM keeps rendering during
/// minimize/restore transitions: the DComp tint layer painted opaque, plus a
/// solid accent fill (a plain gradient accent is composed as a static fill, so
/// unlike the acrylic accent it survives window animations).
#[cfg(target_os = "windows")]
pub(super) fn apply_host_material_solid(hwnd_key: isize, dark: bool) {
    super::host_backdrop::set_solid(hwnd_key, dark);
    apply_solid_accent(hwnd_key as HWND, dark);
    // Best-effort barrier so the cover is composed before the transition starts.
    unsafe {
        let _ = DwmFlush();
    }
}

#[cfg(target_os = "windows")]
fn apply_solid_accent(hwnd: HWND, dark: bool) {
    let Some(set_window_composition_attribute) = resolve_set_window_composition_attribute() else {
        return;
    };
    let mut policy = AccentPolicy {
        state: ACCENT_ENABLE_GRADIENT,
        flags: 0,
        gradient_color: pack_abgr(host_mica_alt_cover(dark)),
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut _ as _,
        size: std::mem::size_of_val(&policy),
    };
    unsafe {
        let _ = set_window_composition_attribute(hwnd, &mut data);
    }
}

#[cfg(target_os = "windows")]
fn apply_windows_host_mica_alt_hwnd(hwnd: HWND, dark: bool) -> Result<(), String> {
    const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
    const DWMSBT_NONE: u32 = 1;
    let dark_mode: BOOL = dark as BOOL;
    let theme_changed = LAST_SWCA_THEME.lock().get(&(hwnd as isize)) != Some(&dark);

    // Pin the DWM system backdrop off. It defaults to AUTO, and while the
    // accent policy is being replaced during a theme switch DWM composes the
    // auto backdrop (real Mica) as an interim material — the mid-switch flash
    // between the light and dark self-maintained Mica Alt tints.
    unsafe {
        let backdrop_none: u32 = DWMSBT_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop_none as *const _ as _,
            std::mem::size_of_val(&backdrop_none) as u32,
        );
    }

    let Some(set_window_composition_attribute) = resolve_set_window_composition_attribute() else {
        disable_windows_live_backdrop(hwnd);
        return Err("SetWindowCompositionAttribute is unavailable".into());
    };
    let mut policy = AccentPolicy {
        state: ACCENT_ENABLE_ACRYLICBLURBEHIND,
        // Acrylic is the reliable live blur engine; this tint owns the Mica Alt-like visual
        // weight. Do not request legacy accent border flags.
        flags: 0,
        gradient_color: pack_abgr(host_mica_alt_tint(dark)),
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut _ as _,
        size: std::mem::size_of_val(&policy),
    };

    if unsafe { set_window_composition_attribute(hwnd, &mut data) } == 0 {
        disable_windows_live_backdrop(hwnd);
        LAST_SWCA_THEME.lock().remove(&(hwnd as isize));
        return Err("ACCENT_ENABLE_ACRYLICBLURBEHIND failed".into());
    }
    // Flip immersive dark mode only after the new accent tint is live, and only
    // on a real theme change: it repaints the frame, and doing it before the
    // accent swap contributes to the mid-switch flash.
    if theme_changed {
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &dark_mode as *const _ as _,
                std::mem::size_of_val(&dark_mode) as u32,
            );
        }
    }
    LAST_SWCA_THEME.lock().insert(hwnd as isize, dark);
    Ok(())
}

#[cfg(target_os = "windows")]
fn resolve_set_window_composition_attribute() -> Option<SetWindowCompositionAttribute> {
    unsafe {
        let user32 = GetModuleHandleA(b"user32.dll\0".as_ptr());
        if user32.is_null() {
            return None;
        }
        GetProcAddress(user32, b"SetWindowCompositionAttribute\0".as_ptr()).map(|function| {
            std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                SetWindowCompositionAttribute,
            >(function)
        })
    }
}

//TODO: this should be remove.
#[cfg(target_os = "windows")]
fn disable_windows_live_backdrop(hwnd: HWND) {
    let mut policy = AccentPolicy {
        state: ACCENT_DISABLED,
        flags: 0,
        gradient_color: 0,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut _ as _,
        size: std::mem::size_of_val(&policy),
    };
    unsafe {
        if let Some(set_window_composition_attribute) = resolve_set_window_composition_attribute() {
            let _ = set_window_composition_attribute(hwnd, &mut data);
        }
    }
}

/// Force the first DWM composition of the freshly revealed main window.
///
/// The main window is created hidden and only shown once its material exists.
/// It is a DWM alpha-composited transparent window, so if nothing has been
/// composed by reveal time — WebView2 never presented, or DWM dropped the
/// accent policy applied while the window was hidden — the window stays fully
/// invisible (a taskbar button with nothing on screen) until a taskbar
/// thumbnail hover or input forces a present. Nudge every layer while visible:
/// rebuild the frame that was configured while hidden, force the window tree
/// (including the WebView2 child) to paint now, and re-assert the material.
#[cfg(target_os = "windows")]
pub fn force_first_present(window: &WebviewWindow, dark: bool) {
    use windows_sys::Win32::Graphics::Gdi::{
        RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let Ok(handle) = window.hwnd() else {
        return;
    };
    let hwnd = handle.0 as HWND;

    unsafe {
        // The shadow/frame dance ran while the window was hidden; re-evaluate
        // the frame now that it is visible. This must happen before the material
        // is re-asserted because a frame rebuild can clear the SWCA policy.
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }

    // Re-assert the material now that the window is visible. The DComp host
    // instance is rebuilt rather than refreshed: its DWM shared-visual
    // registration was created while the window was hidden/cloaked, and DWM
    // never starts composing that stale registration — updating coordinates on
    // it is not enough (the material only appeared after a manual resize).
    // Re-registering against the now-visible window matches how DWM-owned
    // backdrops behave. On the SWCA fallback re-apply the accent policy, which
    // DWM can drop for windows that were hidden when it was applied.
    if USE_NATIVE_BACKDROP {
        // The native backdrop is a DWM attribute; DWM can drop attributes set
        // while the window was hidden, so re-assert it once visible.
        if OsVersion::current().build >= WINDOWS_11_MIN_BUILD {
            if let Err(error) = apply_windows_native_backdrop(window, dark) {
                log::warn!("Failed to re-assert the native backdrop after reveal: {error}");
            }
        }
    } else {
        let rebuilt = if super::host_backdrop::remove(hwnd as isize) {
            match super::host_backdrop::apply(window, dark) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("Failed to rebuild the DComp host material after reveal: {error}");
                    false
                }
            }
        } else {
            false
        };
        if !rebuilt && OsVersion::current().build >= WINDOWS_11_MIN_BUILD {
            if let Err(error) = apply_windows_host_mica_alt_hwnd(hwnd, dark) {
                log::warn!("Failed to re-assert the SWCA material after reveal: {error}");
            }
        }
    }

    // Mechanically reproduce a user resize: on current Win11 builds DWM does
    // not engage the SWCA acrylic accent on a freshly shown window until its
    // size actually changes (SWP_FRAMECHANGED alone is not enough), leaving
    // the shell without its material until the user resized manually. A 1px
    // grow-and-restore forces that re-engagement before first paint settles.
    unsafe {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsZoomed};
        if IsZoomed(hwnd) == 0 {
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd, &mut rect) != 0 {
                let width = rect.right - rect.left;
                let height = rect.bottom - rect.top;
                let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE;
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, width + 1, height, flags);
                SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, width, height, flags);
            }
        }
    }

    unsafe {
        let _ = DwmFlush();
    }
}

fn system_shell_tint(dark: bool) -> Color {
    if dark {
        Color(24, 24, 28, 220)
    } else {
        Color(242, 242, 244, 205)
    }
}

#[cfg(target_os = "windows")]
fn host_mica_alt_tint(dark: bool) -> Color {
    if dark {
        Color(18, 18, 22, 216)
    } else {
        Color(243, 243, 245, 216)
    }
}

/// The minimize cover: the Mica Alt tint at full opacity.
#[cfg(target_os = "windows")]
fn host_mica_alt_cover(dark: bool) -> Color {
    let Color(r, g, b, _) = host_mica_alt_tint(dark);
    Color(r, g, b, 255)
}

#[cfg(target_os = "windows")]
fn pack_abgr(Color(r, g, b, a): Color) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16) | ((a as u32) << 24)
}

pub fn set_effect_color(
    app: &tauri::AppHandle,
    label: &str,
    r: u8,
    g: u8,
    b: u8,
    a: u8,
) -> Result<(), String> {
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("Window '{}' not found", label))?;
    if let Some(effect_name) =
        WindowConfig::from_label(label).and_then(|preset| preset.window_effect)
    {
        #[cfg(target_os = "windows")]
        if effect_name == "system-shell" {
            return Ok(());
        }
        if let Some(effects) = build_window_effects(&effect_name, Some(Color(r, g, b, a))) {
            window.set_effects(effects).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn acrylic_effects(color: Option<Color>) -> Option<tauri::utils::config::WindowEffectsConfig> {
    let mut builder = EffectsBuilder::new();
    #[cfg(target_os = "windows")]
    {
        builder = builder.effect(Effect::Acrylic);
        if let Some(color) = color {
            builder = builder.color(color);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = color;
        builder = builder
            .effect(Effect::HudWindow)
            .state(EffectState::FollowsWindowActiveState)
            .radius(12.0);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = color;
        return None;
    }
    Some(builder.build())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::{host_mica_alt_cover, host_mica_alt_tint, pack_abgr};

    #[test]
    fn packs_host_mica_alt_tints_as_abgr() {
        assert_eq!(pack_abgr(host_mica_alt_tint(true)), 0xd8161212);
        assert_eq!(pack_abgr(host_mica_alt_tint(false)), 0xd8f5f3f3);
    }

    #[test]
    fn minimize_cover_is_opaque_tint() {
        assert_eq!(pack_abgr(host_mica_alt_cover(true)), 0xff161212);
        assert_eq!(pack_abgr(host_mica_alt_cover(false)), 0xfff5f3f3);
    }
}
