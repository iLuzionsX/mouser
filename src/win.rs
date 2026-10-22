//! Windows: a low-level mouse hook for capture, `SendInput` for injection.
//!
//! The PC keyboard is never captured (it stays local); keys arrive from the
//! Mac as HID codes and are injected as positional scan codes. Injected
//! events carry `INJECTED` plus our `dwExtraInfo` tag and are never
//! recaptured, which is what stops a replay looping between machines.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::Ev;
use crate::kvm::{CLAIMS, Captured, Io, Rect};
use crate::wire::{Msg, scan_from_hid};

/// Stamped into `dwExtraInfo` on injected events: ASCII "mouser".
const INJECT_TAG: usize = 0x6D6F_7573_6572;

/// Every system cursor shape the hide swaps for a blank one.
const CURSOR_SHAPES: [SYSTEM_CURSOR_ID; 13] = [
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_HAND,
    OCR_WAIT,
    OCR_CROSS,
    OCR_SIZEALL,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_UP,
    OCR_NO,
    OCR_APPSTARTING,
];

static SINK: OnceLock<Sender<Ev>> = OnceLock::new();

thread_local! {
    /// The pointer's last known position, including our own warps, so a
    /// hardware move becomes a true relative delta. Hook-thread only.
    static LAST_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

/// Reload the user's cursor scheme, undoing any hide. The swap is global and
/// outlives this process, so this also runs at startup (a previous instance
/// may have been killed while hidden), on panic and on Ctrl+C.
fn restore_system_cursors() {
    // SAFETY: SPI_SETCURSORS takes no buffer.
    unsafe { SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0) };
}

unsafe extern "system" fn on_console_close(_: u32) -> i32 {
    restore_system_cursors();
    0 // let the default handler end the process
}

pub struct Win {
    hidden: bool,
}

pub fn start(tx: Sender<Ev>) -> Result<Win, String> {
    restore_system_cursors();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_system_cursors();
        prev(info);
    }));
    // SAFETY: the handler is a plain function valid for the process lifetime.
    unsafe { SetConsoleCtrlHandler(Some(on_console_close), 1) };
    let _ = SINK.set(tx);

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: the hook procedure is a plain function valid for the
        // process lifetime; a low-level hook needs this thread to pump.
        unsafe {
            let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), std::ptr::null_mut(), 0);
            let _ = ready_tx.send(!hook.is_null());
            let mut msg = std::mem::zeroed::<MSG>();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                DispatchMessageW(&msg);
            }
        }
    });
    match ready_rx.recv() {
        Ok(true) => Ok(Win { hidden: false }),
        _ => Err("SetWindowsHookExW(WH_MOUSE_LL) failed".into()),
    }
}

/// A button or wheel record, mapped from the hook message.
fn mouse_msg(msg: u32, data: u32) -> Option<Msg> {
    let high = (data >> 16) as u16;
    Some(match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP => Msg::Button(0, msg == WM_LBUTTONDOWN),
        WM_RBUTTONDOWN | WM_RBUTTONUP => Msg::Button(1, msg == WM_RBUTTONDOWN),
        WM_MBUTTONDOWN | WM_MBUTTONUP => Msg::Button(2, msg == WM_MBUTTONDOWN),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match high {
            XBUTTON1 => Msg::Button(3, msg == WM_XBUTTONDOWN),
            XBUTTON2 => Msg::Button(4, msg == WM_XBUTTONDOWN),
            _ => return None,
        },
        // The wheel delta is the signed high word.
        WM_MOUSEWHEEL => Msg::Scroll(0, high as i16),
        WM_MOUSEHWHEEL => Msg::Scroll(high as i16, 0),
        _ => return None,
    })
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: passes the hook's own arguments along unchanged.
    let next = || unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    if code < 0 {
        return next();
    }
    // SAFETY: for a non-negative code, lparam points to a valid
    // MSLLHOOKSTRUCT for the duration of the call.
    let Some(h) = (unsafe { (lparam as *const MSLLHOOKSTRUCT).as_ref() }) else {
        return next();
    };
    let pt = (h.pt.x, h.pt.y);
    let prev = LAST_POS.replace(Some(pt));
    if h.flags & LLMHF_INJECTED != 0 || h.dwExtraInfo == INJECT_TAG {
        return next();
    }
    let Some(sink) = SINK.get() else {
        return next();
    };
    let msg = wparam as u32;
    if msg == WM_MOUSEMOVE {
        let (dx, dy) = prev.map_or((0, 0), |(px, py)| (pt.0 - px, pt.1 - py));
        let (x, y, dx, dy) = (pt.0.into(), pt.1.into(), dx.into(), dy.into());
        let _ = sink.send(Ev::Local(Captured::Move { x, y, dx, dy }));
    } else if let Some(m) = mouse_msg(msg, h.mouseData) {
        if CLAIMS.claim(m) && sink.send(Ev::Local(Captured::Input(m))).is_ok() {
            return 1;
        }
    }
    // Motion always stays local, so the hook keeps reporting positions.
    next()
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn key(scan: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if scan & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan & 0xFF,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn send(input: INPUT) {
    // SAFETY: one initialized record, copied before SendInput returns.
    if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } != 1 {
        log!("SendInput failed: {}", std::io::Error::last_os_error());
    }
}

impl Io for Win {
    const PARK: bool = true;

    fn screen(&self) -> Rect {
        // SAFETY: plain metric queries.
        let m = |i| f64::from(unsafe { GetSystemMetrics(i) });
        Rect::new(
            m(SM_XVIRTUALSCREEN),
            m(SM_YVIRTUALSCREEN),
            m(SM_CXVIRTUALSCREEN),
            m(SM_CYVIRTUALSCREEN),
        )
    }

    fn inject(&mut self, msg: Msg) {
        let (flags, data) = match msg {
            Msg::Button(0, d) => (if d { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0),
            Msg::Button(1, d) => (if d { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0),
            Msg::Button(2, d) => (if d { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0),
            Msg::Button(b, d) => (
                if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP },
                if b == 3 { XBUTTON1 } else { XBUTTON2 }.into(),
            ),
            Msg::Scroll(dx, dy) => {
                if dx != 0 {
                    send(mouse(0, 0, dx.into(), MOUSEEVENTF_HWHEEL));
                }
                (MOUSEEVENTF_WHEEL, dy.into())
            }
            Msg::Key(hid, down) => {
                if let Some(scan) = scan_from_hid(hid) {
                    send(key(scan, down));
                }
                return;
            }
            _ => return,
        };
        if data != 0 || flags != MOUSEEVENTF_WHEEL {
            send(mouse(0, 0, data, flags));
        }
    }

    /// Absolute move across the whole virtual desktop, in 0..=65535 units.
    fn warp(&mut self, x: f64, y: f64) {
        let s = self.screen();
        let norm = |v: f64, lo: f64, hi: f64| {
            let t = if hi > lo { (v - lo) / (hi - lo) } else { 0.0 };
            (t.clamp(0.0, 1.0) * 65535.0).round() as i32
        };
        let flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        send(mouse(norm(x, s.left, s.right), norm(y, s.top, s.bottom), 0, flags));
    }

    // ShowCursor only affects the calling thread's windows, so the hide
    // swaps every system cursor shape for a blank 1x1 one instead.
    fn hide(&mut self) {
        if std::mem::replace(&mut self.hidden, true) {
            return;
        }
        // All-ones AND mask over an all-zeros XOR mask leaves the screen as is.
        let (and_mask, xor_mask) = ([0xFFu8; 4], [0u8; 4]);
        // SAFETY: the masks outlive CreateBitmap, and SetSystemCursor takes
        // ownership of each cursor it accepts.
        unsafe {
            let info = ICONINFO {
                fIcon: 0,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: CreateBitmap(1, 1, 1, 1, and_mask.as_ptr().cast()),
                hbmColor: CreateBitmap(1, 1, 1, 1, xor_mask.as_ptr().cast()),
            };
            for shape in CURSOR_SHAPES {
                let blank = CreateIconIndirect(&info);
                if !blank.is_null() {
                    SetSystemCursor(blank, shape);
                }
            }
            DeleteObject(info.hbmMask);
            DeleteObject(info.hbmColor);
        }
    }

    fn show(&mut self) {
        if std::mem::replace(&mut self.hidden, false) {
            restore_system_cursors();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_messages_map_to_buttons_and_wheel_units() {
        assert_eq!(mouse_msg(WM_XBUTTONDOWN, 0x0001_0000), Some(Msg::Button(3, true)));
        assert_eq!(mouse_msg(WM_XBUTTONUP, 0x0002_0000), Some(Msg::Button(4, false)));
        assert_eq!(mouse_msg(WM_MOUSEWHEEL, 0xFF88_0000), Some(Msg::Scroll(0, -120)));
        assert_eq!(mouse_msg(WM_MOUSEHWHEEL, 0x0078_0000), Some(Msg::Scroll(120, 0)));
        assert_eq!(mouse_msg(WM_RBUTTONUP, 0), Some(Msg::Button(1, false)));
        assert_eq!(mouse_msg(WM_MOUSEMOVE, 0), None);
    }

    #[test]
    fn injected_keys_are_tagged_extended_scan_codes() {
        // SAFETY: each record was built with the arm being read.
        unsafe {
            let left = key(0xE04B, false).Anonymous.ki;
            assert_eq!(left.wScan, 0x4B);
            assert_eq!(left.dwFlags, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP);
            assert_eq!(left.dwExtraInfo, INJECT_TAG);
            assert_eq!(key(0x1E, true).Anonymous.ki.dwFlags, KEYEVENTF_SCANCODE);
            assert_eq!(mouse(0, 0, 1, MOUSEEVENTF_XDOWN).Anonymous.mi.dwExtraInfo, INJECT_TAG);
        }
    }
}
