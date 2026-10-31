//! The computer's touch screen in device mode and gesture mode: while AAE's
//! window is in front, a window covering each touch screen takes its touches
//! and pen contacts and passes them to the device's touchscreen, scaled to
//! its screen as the user sees it, so the device's screen reader recognises
//! the gestures itself, with as many fingers as the touch screen tracks.
//!
//! - The covering window is almost fully transparent and is never activated,
//!   so the keyboard stays with AAE's window.
//! - A mouse click on it hides it until AAE's window is activated again, so
//!   the mouse can reach the windows under it.
//! - Windows' edge swipes are turned off over it. Windows' three- and
//!   four-finger gestures, and screen readers' touch interaction, such as
//!   NVDA's, take touches before any window does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aae_ffi::{ScreenTouch, Session};
use windows::Win32::Foundation::{
    COLORREF, HWND, LPARAM, LRESULT, PROPERTYKEY, RECT, VARIANT_TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_BOOL;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Controls::{
    FEEDBACK_GESTURE_PRESSANDTAP, FEEDBACK_PEN_BARRELVISUALIZATION, FEEDBACK_PEN_DOUBLETAP,
    FEEDBACK_PEN_PRESSANDHOLD, FEEDBACK_PEN_RIGHTTAP, FEEDBACK_PEN_TAP,
    FEEDBACK_TOUCH_CONTACTVISUALIZATION, FEEDBACK_TOUCH_DOUBLETAP, FEEDBACK_TOUCH_PRESSANDHOLD,
    FEEDBACK_TOUCH_RIGHTTAP, FEEDBACK_TOUCH_TAP, POINTER_DEVICE_INFO,
    POINTER_DEVICE_TYPE_INTEGRATED_PEN, POINTER_DEVICE_TYPE_TOUCH, SetWindowFeedbackSetting,
};
use windows::Win32::UI::Input::Pointer::{
    GetPointerDevices, GetPointerInfo, GetPointerType, POINTER_FLAG_CANCELED,
    POINTER_FLAG_INCONTACT, POINTER_INFO,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{GUID, w};

use crate::ui;

/// Turns off Windows' edge swipes over a window covering its screen.
const EDGE_GESTURE_DISABLE_TOUCH_WHEN_FULLSCREEN: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x32ce38b2_2c9a_41b1_9bc5_b3784394aa44),
    pid: 2,
};

/// Mouse messages Windows made from a touch or pen contact carry this.
const PEN_OR_TOUCH_SIGNATURE: usize = 0xFF51_5700;

struct Cover {
    hwnd: HWND,
    rect: RECT,
}

/// A finger on the device: its number there, and where it is.
#[derive(Clone, Copy)]
struct Finger {
    id: i32,
    x: f64,
    y: f64,
}

struct State {
    covers: Vec<Cover>,
    hook: HWINEVENTHOOK,
    queue: tokio::sync::mpsc::UnboundedSender<Vec<ScreenTouch>>,
    /// Fingers down, by Windows' pointer id.
    fingers: HashMap<u32, Finger>,
    /// Hidden by a mouse click until AAE's window is activated again.
    clicked_away: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Passes the touch screens' touches to the device, if the computer has a
/// touch screen. Call on the window thread.
pub fn start(session: Arc<Session>) {
    stop();
    let monitors = touch_monitors();
    if monitors.is_empty() {
        return;
    }
    register_class();
    let covers: Vec<Cover> = monitors
        .into_iter()
        .filter_map(|rect| Some(Cover { hwnd: cover(&rect)?, rect }))
        .collect();
    if covers.is_empty() {
        return;
    }
    // Touches go through one queue, so they reach the device in order.
    let (queue, mut touches) = tokio::sync::mpsc::unbounded_channel::<Vec<ScreenTouch>>();
    crate::app::spawn(async move {
        while let Some(touches) = touches.recv().await {
            if let Err(e) = session.clone().screen_touches(touches).await {
                tracing::debug!("touch: {e}");
            }
        }
    });
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            covers,
            hook,
            queue,
            fingers: HashMap::new(),
            clicked_away: false,
        })
    });
    update();
}

/// Lifts any fingers still down and gives the touch screens back to Windows.
pub fn stop() {
    let Some(mut state) = STATE.with(|s| s.borrow_mut().take()) else {
        return;
    };
    lift_all(&mut state);
    unsafe {
        if !state.hook.is_invalid() {
            let _ = UnhookWinEvent(state.hook);
        }
        for cover in &state.covers {
            let _ = DestroyWindow(cover.hwnd);
        }
    }
}

/// The rectangles of the monitors with a touch screen or a pen.
fn touch_monitors() -> Vec<RECT> {
    let mut count = 0u32;
    if unsafe { GetPointerDevices(&mut count, None) }.is_err() || count == 0 {
        return Vec::new();
    }
    let mut devices = vec![POINTER_DEVICE_INFO::default(); count as usize];
    if unsafe { GetPointerDevices(&mut count, Some(devices.as_mut_ptr())) }.is_err() {
        return Vec::new();
    }
    devices.truncate(count as usize);
    let mut monitors: Vec<HMONITOR> = Vec::new();
    for device in &devices {
        if matches!(
            device.pointerDeviceType,
            POINTER_DEVICE_TYPE_TOUCH | POINTER_DEVICE_TYPE_INTEGRATED_PEN
        ) && !device.monitor.is_invalid()
            && !monitors.contains(&device.monitor)
        {
            monitors.push(device.monitor);
        }
    }
    monitors
        .into_iter()
        .filter_map(|monitor| {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe { GetMonitorInfoW(monitor, &mut info) }
                .as_bool()
                .then_some(info.rcMonitor)
        })
        .collect()
}

fn register_class() {
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: ui::hinstance(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: w!("AAETouchScreen"),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&class) };
}

/// Makes the window covering one monitor, hidden.
fn cover(rect: &RECT) -> Option<HWND> {
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
            w!("AAETouchScreen"),
            w!("Android touch screen"),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(ui::hinstance()),
            None,
        )
    }
    .ok()?;
    unsafe {
        // Fully transparent windows don't take touches.
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA);
        let off = windows::core::BOOL::from(false);
        for feedback in [
            FEEDBACK_TOUCH_CONTACTVISUALIZATION,
            FEEDBACK_PEN_BARRELVISUALIZATION,
            FEEDBACK_PEN_TAP,
            FEEDBACK_PEN_DOUBLETAP,
            FEEDBACK_PEN_PRESSANDHOLD,
            FEEDBACK_PEN_RIGHTTAP,
            FEEDBACK_TOUCH_TAP,
            FEEDBACK_TOUCH_DOUBLETAP,
            FEEDBACK_TOUCH_PRESSANDHOLD,
            FEEDBACK_TOUCH_RIGHTTAP,
            FEEDBACK_GESTURE_PRESSANDTAP,
        ] {
            let _ = SetWindowFeedbackSetting(
                hwnd,
                feedback,
                0,
                std::mem::size_of::<windows::core::BOOL>() as u32,
                Some(&off as *const _ as *const _),
            );
        }
        if let Ok(store) = SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) {
            let mut value = PROPVARIANT::default();
            (*value.Anonymous.Anonymous).vt = VT_BOOL;
            (*value.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_TRUE;
            let _ = store.SetValue(&EDGE_GESTURE_DISABLE_TOUCH_WHEN_FULLSCREEN, &value);
        }
    }
    Some(hwnd)
}

/// Shows the covering windows while AAE's window is in front, and hides
/// them, lifting any fingers, while it isn't.
fn update() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(state) = s.as_mut() else { return };
        let in_front = unsafe { GetForegroundWindow() } == ui::main_window();
        if !in_front {
            state.clicked_away = false;
        }
        let show = in_front && !state.clicked_away;
        if !show {
            lift_all(state);
        }
        for cover in &state.covers {
            unsafe {
                if show {
                    let _ = SetWindowPos(
                        cover.hwnd,
                        Some(HWND_TOPMOST),
                        cover.rect.left,
                        cover.rect.top,
                        cover.rect.right - cover.rect.left,
                        cover.rect.bottom - cover.rect.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                } else {
                    let _ = ShowWindow(cover.hwnd, SW_HIDE);
                }
            }
        }
    });
}

fn lift_all(state: &mut State) {
    let lifted: Vec<ScreenTouch> = state
        .fingers
        .drain()
        .map(|(_, f)| ScreenTouch {
            id: f.id,
            x: f.x,
            y: f.y,
            down: false,
        })
        .collect();
    if !lifted.is_empty() {
        let _ = state.queue.send(lifted);
    }
}

unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if let Err(panic) = std::panic::catch_unwind(update) {
        crate::log_panic(&panic);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    std::panic::catch_unwind(|| unsafe { handle(hwnd, msg, wparam, lparam) }).unwrap_or_else(
        |panic| {
            crate::log_panic(&panic);
            LRESULT(0)
        },
    )
}

unsafe fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP | WM_POINTERCAPTURECHANGED => {
            let pointer = (wparam.0 & 0xFFFF) as u32;
            if pointer_moved(hwnd, msg, pointer) {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            let extra = unsafe { GetMessageExtraInfo() }.0 as usize;
            if extra & 0xFFFF_FF00 != PEN_OR_TOUCH_SIGNATURE {
                STATE.with(|s| {
                    if let Some(state) = s.borrow_mut().as_mut() {
                        state.clicked_away = true;
                    }
                });
                update();
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Passes a touch or pen contact to the device. Returns false for other
/// pointers, which Windows handles.
fn pointer_moved(hwnd: HWND, msg: u32, pointer: u32) -> bool {
    let mut kind = POINTER_INPUT_TYPE::default();
    if unsafe { GetPointerType(pointer, &mut kind) }.is_err() || !matches!(kind, PT_TOUCH | PT_PEN)
    {
        return false;
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(state) = s.as_mut() else { return true };
        let Some(rect) = state.covers.iter().find(|c| c.hwnd == hwnd).map(|c| c.rect) else {
            return true;
        };
        let mut info = POINTER_INFO::default();
        let known = unsafe { GetPointerInfo(pointer, &mut info) }.is_ok();
        let down = known
            && msg != WM_POINTERUP
            && msg != WM_POINTERCAPTURECHANGED
            && info.pointerFlags.contains(POINTER_FLAG_INCONTACT)
            && !info.pointerFlags.contains(POINTER_FLAG_CANCELED);
        let finger = match state.fingers.get(&pointer).copied() {
            Some(finger) => finger,
            // A pen hovering, or a contact that began before AAE's window was in front.
            None if !down => return true,
            None => {
                let mut id = 0;
                while state.fingers.values().any(|f| f.id == id) {
                    id += 1;
                }
                Finger { id, x: 0.0, y: 0.0 }
            }
        };
        let finger = if known {
            let at = info.ptPixelLocation;
            Finger {
                x: fraction(at.x, rect.left, rect.right),
                y: fraction(at.y, rect.top, rect.bottom),
                ..finger
            }
        } else {
            finger
        };
        if down {
            state.fingers.insert(pointer, finger);
        } else {
            state.fingers.remove(&pointer);
        }
        let _ = state.queue.send(vec![ScreenTouch {
            id: finger.id,
            x: finger.x,
            y: finger.y,
            down,
        }]);
        true
    })
}

fn fraction(at: i32, start: i32, end: i32) -> f64 {
    if end <= start {
        return 0.0;
    }
    (at - start) as f64 / (end - start) as f64
}
