//! Keeps the Windows 11 host material from flashing transparent while DWM plays
//! the minimize/restore transition.
//!
//! DWM stops live-sampling custom backdrops (SWCA accent blur, the DComp
//! underlay) while a window transition animates, so every material pixel would
//! show the desktop through the transparent webview surface for the duration of
//! the animation. Native system backdrops fall back to a solid fill in that
//! window — which is the Electron `backgroundMaterial` behavior. This guard
//! mirrors it: right before the window goes iconic the material is swapped for
//! an opaque solid cover, and the live material returns once the restore
//! transition has finished.

use std::{collections::HashMap, sync::LazyLock};

use parking_lot::Mutex;
use tauri::WebviewWindow;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowThreadProcessId, IsIconic, KillTimer, SetTimer, SC_MINIMIZE, SIZE_MAXIMIZED,
            SIZE_MINIMIZED, SIZE_RESTORED, SWP_NOMOVE, WINDOWPOS, WM_NCDESTROY, WM_SIZE,
            WM_SYSCOMMAND, WM_TIMER, WM_WINDOWPOSCHANGING,
        },
    },
};

const SUBCLASS_ID: usize = 0x4d53_4c52;
const RESTORE_TIMER_ID: usize = 0x4d53_4c52;
/// The DWM restore transition runs ~250ms; hold the opaque cover past it.
const RESTORE_TIMER_MS: u32 = 350;
/// Windows parks minimized windows at this classic iconic position.
const ICONIC_POS: i32 = -32000;

struct GuardState {
    dark: bool,
    solid: bool,
}

static STATES: LazyLock<Mutex<HashMap<isize, GuardState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Install the guard on a window carrying the host material, or refresh the
/// stored theme if it is already installed. Subclassing must happen on the
/// window's own thread, so off-thread callers are rerouted.
pub fn install(window: &WebviewWindow, dark: bool) {
    let Ok(handle) = window.hwnd() else {
        return;
    };
    let hwnd_key = handle.0 as isize;
    if on_window_thread(hwnd_key) {
        install_on_window_thread(hwnd_key, dark);
    } else if let Err(error) =
        window.run_on_main_thread(move || install_on_window_thread(hwnd_key, dark))
    {
        log::warn!("Failed to queue minimize guard install: {error}");
    }
}

/// Remove the guard when the window leaves the host material path.
pub fn uninstall(window: &WebviewWindow) {
    let Ok(handle) = window.hwnd() else {
        return;
    };
    let hwnd_key = handle.0 as isize;
    if on_window_thread(hwnd_key) {
        uninstall_on_window_thread(hwnd_key);
    } else {
        let _ = window.run_on_main_thread(move || uninstall_on_window_thread(hwnd_key));
    }
}

/// While the window is iconic the opaque cover owns the surface: record the new
/// theme and repaint the cover instead of applying the live material (which
/// returns on restore). Returns true when the caller should skip the live path.
pub fn refresh_solid_if_minimized(hwnd_key: isize, dark: bool) -> bool {
    if !unsafe { IsIconic(as_hwnd(hwnd_key)) }.as_bool() {
        return false;
    }
    let installed = {
        let mut states = STATES.lock();
        match states.get_mut(&hwnd_key) {
            Some(state) => {
                state.dark = dark;
                state.solid = true;
                true
            }
            None => false,
        }
    };
    if installed {
        super::effects::apply_host_material_solid(hwnd_key, dark);
    }
    installed
}

fn as_hwnd(hwnd_key: isize) -> HWND {
    HWND(hwnd_key as *mut core::ffi::c_void)
}

fn on_window_thread(hwnd_key: isize) -> bool {
    unsafe { GetWindowThreadProcessId(as_hwnd(hwnd_key), None) == GetCurrentThreadId() }
}

fn install_on_window_thread(hwnd_key: isize, dark: bool) {
    let mut states = STATES.lock();
    if let Some(state) = states.get_mut(&hwnd_key) {
        state.dark = dark;
        return;
    }
    let installed =
        unsafe { SetWindowSubclass(as_hwnd(hwnd_key), Some(guard_proc), SUBCLASS_ID, 0) }.as_bool();
    if installed {
        states.insert(hwnd_key, GuardState { dark, solid: false });
    } else {
        log::warn!("Failed to subclass the window for the minimize material guard");
    }
}

fn uninstall_on_window_thread(hwnd_key: isize) {
    if STATES.lock().remove(&hwnd_key).is_some() {
        unsafe {
            let _ = KillTimer(Some(as_hwnd(hwnd_key)), RESTORE_TIMER_ID);
            let _ = RemoveWindowSubclass(as_hwnd(hwnd_key), Some(guard_proc), SUBCLASS_ID);
        }
    }
}

fn enter_solid(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID);
    }
    let hwnd_key = hwnd.0 as isize;
    let pending_dark = {
        let mut states = STATES.lock();
        match states.get_mut(&hwnd_key) {
            Some(state) if !state.solid => {
                state.solid = true;
                Some(state.dark)
            }
            _ => None,
        }
    };
    if let Some(dark) = pending_dark {
        super::effects::apply_host_material_solid(hwnd_key, dark);
    }
}

fn exit_solid(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID);
    }
    let hwnd_key = hwnd.0 as isize;
    let pending_dark = {
        let mut states = STATES.lock();
        match states.get_mut(&hwnd_key) {
            Some(state) if state.solid => {
                state.solid = false;
                Some(state.dark)
            }
            _ => None,
        }
    };
    if let Some(dark) = pending_dark {
        super::effects::apply_host_material_live(hwnd_key, dark);
    }
}

fn is_solid(hwnd: HWND) -> bool {
    STATES
        .lock()
        .get(&(hwnd.0 as isize))
        .map(|state| state.solid)
        .unwrap_or(false)
}

unsafe extern "system" fn guard_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    match message {
        WM_SYSCOMMAND if wparam.0 & 0xFFF0 == SC_MINIMIZE as usize => {
            // Swap in the opaque cover before DefWindowProc starts the minimize,
            // so the frame DWM animates is already solid.
            enter_solid(hwnd);
            let result = DefSubclassProc(hwnd, message, wparam, lparam);
            // A vetoed minimize never became iconic; put the live material back.
            if !IsIconic(hwnd).as_bool() {
                exit_solid(hwnd);
            }
            return result;
        }
        WM_WINDOWPOSCHANGING => {
            // Programmatic minimize (ShowWindow) skips WM_SYSCOMMAND; the move to
            // the iconic parking position is the earliest cross-path signal.
            if let Some(pos) = (lparam.0 as *const WINDOWPOS).as_ref() {
                if !pos.flags.contains(SWP_NOMOVE) && pos.x == ICONIC_POS && pos.y == ICONIC_POS {
                    enter_solid(hwnd);
                }
            }
        }
        WM_SIZE => match wparam.0 as u32 {
            SIZE_MINIMIZED => enter_solid(hwnd),
            SIZE_RESTORED | SIZE_MAXIMIZED if is_solid(hwnd) && !IsIconic(hwnd).as_bool() => {
                // Hold the cover until the DWM restore transition has finished,
                // then bring the live material back.
                SetTimer(Some(hwnd), RESTORE_TIMER_ID, RESTORE_TIMER_MS, None);
            }
            _ => {}
        },
        WM_TIMER if wparam.0 == RESTORE_TIMER_ID => {
            if IsIconic(hwnd).as_bool() {
                let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID);
            } else {
                exit_solid(hwnd);
            }
        }
        WM_NCDESTROY => {
            uninstall_on_window_thread(hwnd.0 as isize);
            super::host_backdrop::remove(hwnd.0 as isize);
            super::effects::forget_swca_theme(hwnd.0 as isize);
            return DefSubclassProc(hwnd, message, wparam, lparam);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, message, wparam, lparam)
}
