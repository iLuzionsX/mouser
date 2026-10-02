//! Windows input capture and injection.
//!
//! Capture uses low-level hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) on a
//! dedicated thread with a message loop, which is required: a low-level hook
//! must be serviced by the thread that installed it.
//!
//! Injection uses `SendInput`, and captured events whose `INJECTED` flag is
//! set are ignored. That check is what stops a replayed event from being
//! captured again and looping forever between the two machines.
//!
//! Key identity is taken from the scan code rather than the virtual key code.
//! Virtual key codes describe the *active layout* (`A` and `Q` swap on AZERTY),
//! so the peer would receive a different key than the one actually pressed.
//! Scan codes are positional and stable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use mouser_core::layout::Rect;
use mouser_core::protocol::{Button, HidKey, InputEvent, Modifiers};
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
    PostThreadMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetWindowsHookExW, ShowCursor, TranslateMessage, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::events::CapturedEvent;
use crate::keymap;
use crate::{InputBackend, InputError, ScreenInfo};

/// Private application message used to stop the hook thread's message loop.
const WM_APP_QUIT: u32 = 0x4D1A;

/// `SendInput` treats a `dx`/`dy` of this as "no movement", which is how a
/// button or wheel event is expressed without also moving the cursor.
const NO_MOVE: i32 = -1;

/// One notch of wheel travel in Windows' internal unit.
const WHEEL_DELTA: i32 = 120;

/// Stamped into `dwExtraInfo` on injected events.
///
/// Lets our own events be recognized even where the `INJECTED` flag is not
/// available, and must be non-zero to be distinguishable from "unset".
const INJECT_TAG: usize = 0x6D6F_7573_6572;

/// How many times to nudge `ShowCursor`, which uses a shared counter rather
/// than a boolean.
const SHOW_CURSOR_ATTEMPTS: usize = 16;

pub struct WindowsBackend {
    active: AtomicBool,
    hook_thread: Mutex<Option<HookThread>>,
}

struct HookThread {
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WindowsBackend {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            hook_thread: Mutex::new(None),
        }
    }

    /// Translate an absolute virtual-desktop position into the 0..65535
    /// range `MOUSEEVENTF_ABSOLUTE` expects.
    ///
    /// `MOUSEEVENTF_VIRTUALDESK` makes those coordinates span every monitor
    /// instead of just the primary one, which is required for a multi-monitor
    /// setup to hand off correctly.
    fn absolute_coords(&self, x: f64, y: f64) -> (i32, i32) {
        let bounds = self.screen_info().bounds;
        let norm_x = if bounds.width > 0.0 {
            (x - bounds.left()) / bounds.width
        } else {
            0.0
        };
        let norm_y = if bounds.height > 0.0 {
            (y - bounds.top()) / bounds.height
        } else {
            0.0
        };
        let clamp = |v: f64| ((v.clamp(0.0, 1.0) * 65535.0).round()) as i32;
        (clamp(norm_x), clamp(norm_y))
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBackend for WindowsBackend {
    fn start_capture(&self, sink: Sender<CapturedEvent>) -> Result<(), InputError> {
        if self.active.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let (thread_id_tx, thread_id_rx) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name("mouser-hooks".into())
            .spawn(move || {
                if let Err(e) = hook_loop(&sink, &thread_id_tx) {
                    tracing::error!("input hook loop failed: {e}");
                }
            })
            .map_err(|e| InputError::Hook(e.to_string()))?;

        // The thread reports its id before entering the message loop, so by
        // the time this resolves the hook is installed and pumping.
        let thread_id = thread_id_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| InputError::Hook(e.to_string()))?;

        *self.hook_thread.lock().unwrap() = Some(HookThread {
            thread_id,
            join: Some(join),
        });
        Ok(())
    }

    fn stop_capture(&self) {
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(HookThread { thread_id, join }) = self.hook_thread.lock().unwrap().take() {
            // PostThreadMessage needs the target queue to exist, which only
            // happens once the thread calls GetMessageW. Retry briefly rather
            // than racing it and leaking the thread.
            for _ in 0..50 {
                // SAFETY: post-only call; nothing is shared with the callee.
                if unsafe { PostThreadMessageW(thread_id, WM_APP_QUIT, WPARAM(0), LPARAM(0)) }
                    .is_ok()
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if let Some(join) = join {
                let _ = join.join();
            }
        }
        let _ = self.show_cursor();
    }

    fn inject(&self, event: &InputEvent) -> Result<(), InputError> {
        match *event {
            // A peer sends absolute cursor positions in virtual-desktop
            // coordinates, which is the only representation that can cross a
            // monitor boundary. Deltas would be wrong on any setup where the
            // two machines' monitors differ in size or offset.
            InputEvent::Move { .. } => self.inject_absolute(event),
            InputEvent::Scroll { dx, dy } => self.inject_scroll(dx, dy),
            InputEvent::Button { button, pressed } => self.inject_button(button, pressed),
            InputEvent::Key {
                key,
                pressed,
                mods: _,
            } => self.inject_key(key, pressed),
        }
    }

    fn screen_info(&self) -> ScreenInfo {
        // SAFETY: pure queries, no pointers, no shared state.
        unsafe {
            ScreenInfo {
                bounds: Rect::new(
                    GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
                    GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
                    GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64,
                    GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64,
                ),
            }
        }
    }

    fn cursor_position(&self) -> Option<(f64, f64)> {
        let mut point = POINT::default();
        // SAFETY: `point` is a valid, writable POINT.
        unsafe { GetCursorPos(&mut point) }.ok()?;
        Some((point.x as f64, point.y as f64))
    }

    fn warp_cursor(&self, x: f64, y: f64) -> Result<(), InputError> {
        // `SendInput` marks the event as injected, so the low-level hook drops
        // it instead of forwarding the recentering back to the peer.
        let (cx, cy) = self.cursor_position().unwrap_or((x, y));
        self.inject_absolute(&InputEvent::Move {
            dx: (x - cx).round() as i32,
            dy: (y - cy).round() as i32,
        })
    }

    fn hide_cursor(&self) -> Result<(), InputError> {
        // ShowCursor adjusts a process-wide counter, so it must be driven to
        // zero; other code may be incrementing it concurrently.
        for _ in 0..SHOW_CURSOR_ATTEMPTS {
            // SAFETY: no arguments, no shared memory.
            if unsafe { ShowCursor(false) } < 0 {
                break;
            }
        }
        Ok(())
    }

    fn show_cursor(&self) -> Result<(), InputError> {
        for _ in 0..SHOW_CURSOR_ATTEMPTS {
            // SAFETY: no arguments, no shared memory.
            if unsafe { ShowCursor(true) } < 0 {
                break;
            }
        }
        Ok(())
    }
}

impl WindowsBackend {
    fn inject_absolute(&self, event: &InputEvent) -> Result<(), InputError> {
        let InputEvent::Move { dx, dy } = *event else {
            return Ok(());
        };
        let (cx, cy) = self.cursor_position().unwrap_or((0.0, 0.0));
        let (x, y) = self.absolute_coords(cx + dx as f64, cy + dy as f64);
        send(&[mouse_input(
            x,
            y,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn inject_scroll(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        let mut records = Vec::with_capacity(2);
        if dy != 0 {
            records.push(mouse_input(NO_MOVE, dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            records.push(mouse_input(NO_MOVE, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
        }
        if records.is_empty() {
            return Ok(());
        }
        send(&records)
    }

    fn inject_button(&self, button: Button, pressed: bool) -> Result<(), InputError> {
        let flag = match (button, pressed) {
            (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (Button::Left, false) => MOUSEEVENTF_LEFTUP,
            (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
            (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            // Back/forward need XBUTTON1/XBUTTON2 data, which `SendInput`
            // cannot express. Silently skipped rather than sending a wrong
            // button press.
            (Button::Back, _) | (Button::Forward, _) => return Ok(()),
        };
        send(&[mouse_input(NO_MOVE, 0, flag)])
    }

    fn inject_key(&self, key: HidKey, pressed: bool) -> Result<(), InputError> {
        let Some(native) = keymap::to_native(key) else {
            // Unmapped key: skip rather than guessing a virtual key.
            return Ok(());
        };
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if keymap::needs_extended(native) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[keyboard_input(VIRTUAL_KEY(native as u16), flags)])
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // WHEEL/HWHEEL take their delta in mouseData; the other
                // events ignore the field.
                mouseData: dy as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn keyboard_input(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

/// Send records as one atomic call.
///
/// Batching is a correctness requirement, not an optimization: separate calls
/// for a click can be reordered against a modifier press, producing
/// modifier-clicks on the receiving machine.
fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    // SAFETY: `inputs` is a valid slice of initialized INPUT records, and
    // SendInput copies them synchronously before returning.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(InputError::Inject(format!(
            "SendInput delivered {sent} of {} records",
            inputs.len()
        )))
    }
}

/// Destination for hook callbacks, which are plain `extern "system"` fns and
/// cannot capture state. One hook thread exists per process, so one slot.
fn sink_slot() -> &'static Mutex<Option<Sender<CapturedEvent>>> {
    static SLOT: OnceLock<Mutex<Option<Sender<CapturedEvent>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install both hooks and pump messages until asked to quit.
fn hook_loop(sink: &Sender<CapturedEvent>, thread_id_tx: &Sender<u32>) -> Result<(), InputError> {
    *sink_slot().lock().unwrap() = Some(sink.clone());

    thread_id_tx
        .send(unsafe { GetCurrentThreadId() })
        .map_err(|e| InputError::Hook(e.to_string()))?;

    // SAFETY: both callbacks live in this module and stay valid for the
    // whole lifetime of the process; the hooks are removed before this
    // function returns, so no callback can run after it does.
    unsafe {
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0)
            .map_err(|e| InputError::Hook(format!("mouse hook: {e}")))?;
        let keyboard = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
            Ok(hook) => hook,
            Err(e) => {
                let _ = UnhookWindowsHookEx(mouse);
                return Err(InputError::Hook(format!("keyboard hook: {e}")));
            }
        };

        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0);
            // -1 signals an error, 0 a clean WM_QUIT.
            if result.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(keyboard);
        let _ = UnhookWindowsHookEx(mouse);
    }

    *sink_slot().lock().unwrap() = None;
    Ok(())
}

/// Forward a captured event, if a hook thread is listening.
fn forward(event: CapturedEvent) {
    let slot = sink_slot();
    let guard = slot.lock().expect("sink slot is never poisoned");
    if let Some(sink) = guard.as_ref() {
        // A closed receiver simply means the app is shutting down.
        let _ = sink.send(event);
    }
}

/// True when an event originated from injection rather than a real device.
fn is_injected(injected_flag: bool, extra_info: usize) -> bool {
    injected_flag || extra_info == INJECT_TAG
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: per the WH_MOUSE_LL contract, for a non-negative `code` the
    // lparam points to a valid MOUSE_LLHOOKSTRUCT for the duration of the call.
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let Some(hook) = (lparam.0 as *const MSLLHOOKSTRUCT).as_ref() else {
            return CallNextHookEx(None, code, wparam, lparam);
        };
        if is_injected(hook.flags & LLMHF_INJECTED != 0, hook.dwExtraInfo) {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let (x, y) = (hook.pt.x as f64, hook.pt.y as f64);
        let event = match wparam.0 as u32 {
            WM_MOUSEMOVE => Some(CapturedEvent::Move {
                x,
                y,
                // LL hooks report absolute positions only. Deltas are
                // derived downstream from consecutive positions.
                dx: 0.0,
                dy: 0.0,
            }),
            WM_LBUTTONDOWN => Some(CapturedEvent::Button {
                button: Button::Left,
                pressed: true,
            }),
            WM_LBUTTONUP => Some(CapturedEvent::Button {
                button: Button::Left,
                pressed: false,
            }),
            WM_RBUTTONDOWN => Some(CapturedEvent::Button {
                button: Button::Right,
                pressed: true,
            }),
            WM_RBUTTONUP => Some(CapturedEvent::Button {
                button: Button::Right,
                pressed: false,
            }),
            WM_MBUTTONDOWN => Some(CapturedEvent::Button {
                button: Button::Middle,
                pressed: true,
            }),
            WM_MBUTTONUP => Some(CapturedEvent::Button {
                button: Button::Middle,
                pressed: false,
            }),
            WM_MOUSEWHEEL => Some(CapturedEvent::Scroll {
                dx: 0.0,
                dy: wheel_notches(hook.mouseData),
            }),
            WM_MOUSEHWHEEL => Some(CapturedEvent::Scroll {
                dx: wheel_notches(hook.mouseData),
                dy: 0.0,
            }),
            _ => None,
        };

        if let Some(event) = event {
            forward(event);
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: per the WH_KEYBOARD_LL contract, for a non-negative `code` the
    // lparam points to a valid KBDLLHOOKSTRUCT for the duration of the call.
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let Some(hook) = (lparam.0 as *const KBDLLHOOKSTRUCT).as_ref() else {
            return CallNextHookEx(None, code, wparam, lparam);
        };
        if is_injected(hook.flags.0 & LLKHF_INJECTED.0 != 0, hook.dwExtraInfo) {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let pressed = match wparam.0 as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => true,
            WM_KEYUP | WM_SYSKEYUP => false,
            _ => return CallNextHookEx(None, code, wparam, lparam),
        };

        let extended = hook.flags.0 & (LLKHF_EXTENDED.0 | LLKHF_ALTDOWN.0) != 0;
        if let Some(key) = hid_from_scan(hook.scanCode, extended) {
            forward(CapturedEvent::Key {
                key,
                pressed,
                mods: Modifiers::default(),
            });
        }

        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Decode a Windows wheel `mouseData` value into notches.
///
/// The delta lives in the high word as a *signed* 16-bit value, so scrolling
/// down arrives as a negative number.
fn wheel_notches(mouse_data: u32) -> f64 {
    ((mouse_data >> 16) & 0xFFFF) as i16 as f64
}

/// Scan code set 1 to USB HID usage.
///
/// Windows passes the byte following an E0 prefix with `LLKHF_EXTENDED` set,
/// so E0 codes are matched on the post-E0 byte plus the flag rather than by
/// a synthetic 0xE0 prefix. Codes that carry no reliable positional meaning
/// (the keypad digits, which change with NumLock) are deliberately omitted:
/// guessing would send the wrong key to the peer machine.
fn hid_from_scan(scan: u32, extended: bool) -> Option<HidKey> {
    // Extended (E0-prefixed) codes must be matched first. Several share a
    // byte with a base-1 code -- 0x4B is both Left and keypad 7, 0x4D is Down
    // and keypad 6 -- so an unordered match would resolve them wrongly.
    if extended {
        // Navigation cluster: 0x49 Insert, 0x4A Home, 0x4B PageUp,
        // 0x4C Delete, 0x4D End, 0x4E PageDown, 0x4F Right, 0x50 Left,
        // 0x51 Down, 0x52 Up.
        return match scan & 0xFF {
            0x1C => Some(HidKey(0x28)), // Keypad Enter
            0x1D => Some(HidKey(0xE4)), // RightControl
            0x35 => Some(HidKey(0x54)), // Keypad /
            0x37 => Some(HidKey(0x4F)), // Right
            0x38 => Some(HidKey(0xE6)), // RightAlt
            0x47 => Some(HidKey(0x4A)), // Home
            0x48 => Some(HidKey(0x52)), // Up
            0x4B => Some(HidKey(0x50)), // Left
            0x4D => Some(HidKey(0x51)), // Down
            0x4F => Some(HidKey(0x49)), // Insert
            0x50 => Some(HidKey(0x4D)), // End
            0x51 => Some(HidKey(0x4C)), // Delete
            0x53 => Some(HidKey(0x4E)), // PageDown
            0x5B => Some(HidKey(0x4B)), // LeftGUI
            0x5C => Some(HidKey(0xE7)), // RightGUI
            _ => None,
        };
    }

    let hid = match scan & 0xFF {
        // Main block.
        0x01 => 0x29, // Escape
        0x02 => 0x1E, // 1
        0x03 => 0x1F, // 2
        0x04 => 0x20, // 3
        0x05 => 0x21, // 4
        0x06 => 0x22, // 5
        0x07 => 0x23, // 6
        0x08 => 0x24, // 7
        0x09 => 0x25, // 8
        0x0A => 0x26, // 9
        0x0B => 0x27, // 0
        0x0C => 0x2D, // -
        0x0D => 0x2E, // =
        0x0E => 0x2A, // Backspace
        0x0F => 0x2B, // Tab
        0x10 => 0x14, // Q
        0x11 => 0x1A, // W
        0x12 => 0x08, // E
        0x13 => 0x15, // R
        0x14 => 0x17, // T
        0x15 => 0x1C, // Y
        0x16 => 0x18, // U
        0x17 => 0x0C, // I
        0x18 => 0x12, // O
        0x19 => 0x13, // P
        0x1A => 0x2F, // [
        0x1B => 0x30, // ]
        0x1C => 0x28, // Enter
        0x1D => 0xE0, // LeftControl
        0x1E => 0x04, // A
        0x1F => 0x16, // S
        0x20 => 0x07, // D
        0x21 => 0x09, // F
        0x22 => 0x0A, // G
        0x23 => 0x0B, // H
        0x24 => 0x0D, // J
        0x25 => 0x0E, // K
        0x26 => 0x0F, // L
        0x27 => 0x33, // ;
        0x28 => 0x34, // '
        0x29 => 0x35, // `
        0x2A => 0xE1, // LeftShift
        0x2B => 0x31, // \
        0x2C => 0x1D, // Z
        0x2D => 0x1B, // X
        0x2E => 0x06, // C
        0x2F => 0x19, // V
        0x30 => 0x05, // B
        0x31 => 0x11, // N
        0x32 => 0x10, // M
        0x33 => 0x36, // ,
        0x34 => 0x37, // .
        0x35 => 0x38, // /
        0x36 => 0xE5, // RightShift
        0x37 => 0x55, // Keypad *
        0x38 => 0xE2, // LeftAlt
        0x39 => 0x2C, // Space
        0x3A => 0x39, // CapsLock
        0x3B => 0x3A, // F1
        0x3C => 0x3B, // F2
        0x3D => 0x3C, // F3
        0x3E => 0x3D, // F4
        0x3F => 0x3E, // F5
        0x40 => 0x3F, // F6
        0x41 => 0x40, // F7
        0x42 => 0x41, // F8
        0x43 => 0x42, // F9
        0x44 => 0x43, // F10
        0x45 => 0x53, // NumLock
        0x46 => 0x46, // ScrollLock
        // Keypad. HID usages 0x59..=0x62 are keypad 1..0; 0x63 is keypad dot.
        0x47 => 0x5F, // Keypad 7
        0x48 => 0x60, // Keypad 8
        0x49 => 0x61, // Keypad 9
        0x4A => 0x54, // Keypad /
        0x4B => 0x5C, // Keypad 4
        0x4C => 0x5D, // Keypad 5
        0x4D => 0x5E, // Keypad 6
        0x4E => 0x56, // Keypad -
        0x4F => 0x59, // Keypad 1
        0x50 => 0x5A, // Keypad 2
        0x51 => 0x5B, // Keypad 3
        0x52 => 0x62, // Keypad 0
        0x53 => 0x63, // Keypad .
        0x56 => 0xE3, // LeftGUI
        0x57 => 0x3F, // F11
        0x58 => 0x41, // F12
        _ => return None,
    };
    Some(HidKey(hid as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_notches_handle_negative_deltas() {
        assert_eq!(wheel_notches(0x0001_0000), 1.0);
        assert_eq!(wheel_notches(0xFFFF_0000), -1.0);
        assert_eq!(wheel_notches(0x0002_0000), 2.0);
    }

    #[test]
    fn injection_tag_is_nonzero_and_detected() {
        // A zero dwExtraInfo would be indistinguishable from "unset", so an
        // injected event could be recaptured and loop.
        assert_ne!(INJECT_TAG, 0);
        assert!(is_injected(false, INJECT_TAG));
        assert!(is_injected(true, 0));
        assert!(!is_injected(false, 0));
    }

    #[test]
    fn key_records_carry_the_injection_tag() {
        // SAFETY: INPUT_0 is a union, so reading an arm is unsafe. Each record
        // below was built with the matching arm, so this is well defined.
        unsafe {
            let INPUT_0 { ki } = keyboard_input(VIRTUAL_KEY(0x41), KEYBD_EVENT_FLAGS(0)).Anonymous;
            assert_eq!(ki.dwExtraInfo, INJECT_TAG);
            let INPUT_0 { mi } = mouse_input(0, 0, MOUSEEVENTF_MOVE).Anonymous;
            assert_eq!(mi.dwExtraInfo, INJECT_TAG);
        }
    }

    #[test]
    fn scan_codes_produce_known_hid_usages() {
        assert_eq!(hid_from_scan(0x1E, false), Some(HidKey(0x04))); // A
        assert_eq!(hid_from_scan(0x1C, false), Some(HidKey(0x28))); // Enter
        assert_eq!(hid_from_scan(0x1D, false), Some(HidKey(0xE0))); // LCtrl
        assert_eq!(hid_from_scan(0x1D, true), Some(HidKey(0xE4))); // RCtrl
        assert_eq!(hid_from_scan(0x4B, true), Some(HidKey(0x50))); // Left
        assert_eq!(hid_from_scan(0xE1, false), None); // unmapped
    }

    #[test]
    fn scan_table_agrees_with_the_keymap() {
        // The two tables are written by hand and encode the same physical
        // keys, so they can drift apart silently. Any scan code whose HID
        // usage is in the keymap must name the same key, or a key pressed on
        // one machine would arrive as a different key on the other.
        for scan in 0x00u32..=0xFF {
            for extended in [false, true] {
                let Some(from_scan) = hid_from_scan(scan, extended) else {
                    continue;
                };
                // Only compare keys the keymap knows about; ScanCode-only
                // entries like PrintScreen are legitimately absent.
                if keymap::to_native(from_scan).is_none() {
                    continue;
                }
                // A key present in both tables must agree.
                if let Some(shared) = shared_hid_key(scan, extended) {
                    assert_eq!(
                        from_scan, shared,
                        "scan {scan:#04x} (extended={extended}) maps to \
                         {from_scan:?} in hid_from_scan but {shared:?} in keymap"
                    );
                }
            }
        }
    }

    /// The keymap entry a scan code should correspond to, if there is one.
    ///
    /// Derived by inverting: a scan code and a virtual key code are two
    /// names for the same physical key, so the entry whose Windows virtual
    /// key code matches the scan code's documented pairing.
    fn shared_hid_key(scan: u32, extended: bool) -> Option<HidKey> {
        // The navigation cluster shares bytes between base-1 and E0, so a
        // scan code alone does not determine a virtual key code. Only compare
        // where the mapping is unambiguous.
        if extended {
            return None;
        }
        // Base-1 scan codes and virtual key codes for the alphanumeric block
        // map by an explicit table, not arithmetic: scan 0x02 is the digit 1
        // (VK 0x31) but scan 0x10 is Q (VK 0x51), so an offset would be wrong
        // for one of them.
        let vk = match scan {
            0x02 => 0x31, // 1
            0x03 => 0x32, // 2
            0x04 => 0x33, // 3
            0x05 => 0x34, // 4
            0x06 => 0x35, // 5
            0x07 => 0x36, // 6
            0x08 => 0x37, // 7
            0x09 => 0x38, // 8
            0x0A => 0x39, // 9
            0x0B => 0x30, // 0
            0x10 => 0x51, // Q
            0x11 => 0x57, // W
            0x12 => 0x45, // E
            0x13 => 0x52, // R
            0x14 => 0x54, // T
            0x15 => 0x59, // Y
            0x16 => 0x55, // U
            0x17 => 0x49, // I
            0x18 => 0x4F, // O
            0x19 => 0x50, // P
            0x1E => 0x41, // A
            0x1F => 0x53, // S
            0x20 => 0x44, // D
            0x21 => 0x46, // F
            0x22 => 0x47, // G
            0x23 => 0x48, // H
            0x24 => 0x4A, // J
            0x25 => 0x4B, // K
            0x26 => 0x4C, // L
            0x2C => 0x5A, // Z
            0x2D => 0x58, // X
            0x2E => 0x43, // C
            0x2F => 0x56, // V
            0x30 => 0x42, // B
            0x31 => 0x4E, // N
            0x32 => 0x4D, // M
            _ => return None,
        };
        keymap::from_native(vk)
    }

    #[test]
    fn every_mapped_scan_code_is_a_real_hid_usage() {
        // Guards against the placeholder-style mistakes a hand-written table
        // invites: usage 0x00 is "no event present".
        for scan in 0x00u32..=0xFF {
            for extended in [false, true] {
                if let Some(HidKey(hid)) = hid_from_scan(scan, extended) {
                    assert_ne!(hid, 0x00, "scan {scan:#04x} mapped to HID 0x00");
                    assert!(
                        hid >= 0x04 || (0x28..=0x39).contains(&hid) || hid >= 0x39,
                        "scan {scan:#04x} mapped to suspicious HID {hid:#04x}"
                    );
                }
            }
        }
    }

    #[test]
    fn absolute_coordinates_cover_the_virtual_desktop() {
        let backend = WindowsBackend::new();
        let bounds = backend.screen_info().bounds;

        let (min_x, min_y) = backend.absolute_coords(bounds.left(), bounds.top());
        let (max_x, max_y) = backend.absolute_coords(bounds.right(), bounds.bottom());
        assert_eq!((min_x, min_y), (0, 0));
        assert_eq!((max_x, max_y), (65535, 65535));

        // Out-of-range input must clamp rather than wrap around the desktop.
        let (x, _) = backend.absolute_coords(bounds.right() + 10_000.0, 0.0);
        assert_eq!(x, 65535);
        let (_, y) = backend.absolute_coords(0.0, bounds.top() - 10_000.0);
        assert_eq!(y, 0);
    }
}
