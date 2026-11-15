//! Native Windows controller: translates Apple TV remote buttons and touch gestures
//! into Win32 keyboard/mouse input, media/volume keys, and monitor sleep/wake.
//!
//! Uses a handful of `user32` calls directly (no extra crates); no special
//! permission is required on Windows (unlike macOS Accessibility).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atv_core::{AtvDelegate, EventKind, TextOp, TouchPhase, TrackpadMode};
use tracing::{debug, info, warn};

#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct KeybdInput {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputUnion {
    #[allow(dead_code)]
    mi: MouseInput,
    ki: KeybdInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Input {
    kind: u32,
    u: InputUnion,
}

#[link(name = "user32")]
extern "system" {
    fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
    fn mouse_event(flags: u32, dx: i32, dy: i32, data: u32, extra: usize);
    fn SendInput(count: u32, inputs: *const Input, cb_size: i32) -> u32;
    fn GetCursorPos(p: *mut Point) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SetProcessDPIAware() -> i32;
}

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x1;
const KEYEVENTF_KEYUP: u32 = 0x2;
const KEYEVENTF_UNICODE: u32 = 0x4;
const MOUSEEVENTF_MOVE: u32 = 0x1;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x2;
const MOUSEEVENTF_LEFTUP: u32 = 0x4;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const WHEEL_DELTA: i32 = 120;

const VK_BACK: u8 = 0x08;
const VK_RETURN: u8 = 0x0D;
const VK_VOLUME_MUTE: u8 = 0xAD;
const VK_VOLUME_DOWN: u8 = 0xAE;
const VK_VOLUME_UP: u8 = 0xAF;
const VK_MEDIA_NEXT: u8 = 0xB0;
const VK_MEDIA_PREV: u8 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u8 = 0xB3;

/// Windows virtual-key codes for simple remote buttons.
/// `home` and `play_pause` are reserved for vertical mouse-wheel scrolling.
fn vk_for_button(name: &str) -> Option<u8> {
    Some(match name {
        "up" => 0x26,
        "down" => 0x28,
        "left" | "rewind" => 0x25,
        "right" | "fast_forward" => 0x27,
        "menu" => 0x1B,                         // Escape
        "channel_up" | "page_up" => 0x21,       // Page Up
        "channel_down" | "page_down" => 0x22,   // Page Down
        "guide" => 0x09,                        // Tab
        "play" | "pause" => VK_MEDIA_PLAY_PAUSE,
        "next" | "next_track" => VK_MEDIA_NEXT,
        "prev" | "previous" | "prev_track" | "previous_track" => VK_MEDIA_PREV,
        "volume_up" => VK_VOLUME_UP,
        "volume_down" => VK_VOLUME_DOWN,
        "mute" => VK_VOLUME_MUTE,
        _ => return None,
    })
}

fn scroll_delta_for_button(name: &str) -> Option<i32> {
    match name {
        "play_pause" => Some(WHEEL_DELTA),
        "home" => Some(-WHEEL_DELTA),
        _ => None,
    }
}

fn mouse_wheel(delta: i32) {
    unsafe {
        mouse_event(MOUSEEVENTF_WHEEL, 0, 0, delta as u32, 0);
    }
}

fn key_input(vk: u16, scan: u16, flags: u32) -> Input {
    Input {
        kind: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KeybdInput {
                w_vk: vk,
                w_scan: scan,
                dw_flags: flags,
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}

fn send_inputs(inputs: &[Input]) {
    if inputs.is_empty() {
        return;
    }
    let size = std::mem::size_of::<Input>() as i32;
    for chunk in inputs.chunks(200) {
        unsafe {
            SendInput(chunk.len() as u32, chunk.as_ptr(), size);
        }
    }
}

/// Update tracked remote input buffer and return `(backspaces, text_to_insert)`.
fn plan_text_edit(buf: &mut String, op: &TextOp) -> (u32, String) {
    match op {
        TextOp::Insert(text) => {
            buf.push_str(text);
            (0, text.clone())
        }
        TextOp::Delete(count) => {
            for _ in 0..*count {
                if buf.pop().is_none() {
                    break;
                }
            }
            (*count, String::new())
        }
        TextOp::Replace(target) => {
            let common_chars = buf
                .chars()
                .zip(target.chars())
                .take_while(|(a, b)| a == b)
                .count();
            let old_chars = buf.chars().count();
            let backspaces = u32::try_from(old_chars - common_chars).unwrap_or(u32::MAX);
            let insert: String = target.chars().skip(common_chars).collect();
            *buf = target.clone();
            (backspaces, insert)
        }
    }
}

fn build_text_inputs(backspaces: u32, text: &str) -> Vec<Input> {
    let cap = (backspaces as usize).saturating_add(text.len()) * 2;
    let mut inputs = Vec::with_capacity(cap.min(1024));
    for _ in 0..backspaces {
        inputs.push(key_input(VK_BACK as u16, 0, 0));
        inputs.push(key_input(VK_BACK as u16, 0, KEYEVENTF_KEYUP));
    }
    let mut prev_cr = false;
    for ch in text.chars() {
        if ch == '\r' {
            inputs.push(key_input(VK_RETURN as u16, 0, 0));
            inputs.push(key_input(VK_RETURN as u16, 0, KEYEVENTF_KEYUP));
            prev_cr = true;
            continue;
        }
        if ch == '\n' {
            if !prev_cr {
                inputs.push(key_input(VK_RETURN as u16, 0, 0));
                inputs.push(key_input(VK_RETURN as u16, 0, KEYEVENTF_KEYUP));
            }
            prev_cr = false;
            continue;
        }
        prev_cr = false;
        let mut utf16 = [0u16; 2];
        for &unit in ch.encode_utf16(&mut utf16) {
            inputs.push(key_input(0, unit, KEYEVENTF_UNICODE));
            inputs.push(key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
    }
    inputs
}

fn press_key(vk: u8) {
    // Arrows, Home, PgUp/PgDn and media keys are "extended" keys; Esc/Tab/Enter are not.
    let ext = if matches!(vk, 0x09 | 0x0D | 0x1B) { 0 } else { KEYEVENTF_EXTENDEDKEY };
    unsafe {
        keybd_event(vk, 0, ext, 0);
        keybd_event(vk, 0, ext | KEYEVENTF_KEYUP, 0);
    }
}

fn cursor_pos() -> (f64, f64) {
    let mut p = Point::default();
    unsafe { GetCursorPos(&mut p) };
    (p.x as f64, p.y as f64)
}

/// Bounds of the whole virtual desktop (all monitors): (left, top, right, bottom).
fn virtual_screen() -> (f64, f64, f64, f64) {
    unsafe {
        let (x, y) = (GetSystemMetrics(76), GetSystemMetrics(77));
        let (w, h) = (GetSystemMetrics(78), GetSystemMetrics(79));
        (x as f64, y as f64, (x + w - 1) as f64, (y + h - 1) as f64)
    }
}

fn set_monitor_power(on: bool) {
    unsafe {
        if on {
            // Any user input wakes the display.
            mouse_event(MOUSEEVENTF_MOVE, 1, 0, 0, 0);
            mouse_event(MOUSEEVENTF_MOVE, -1, 0, 0, 0);
        } else {
            // HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2 = off
            PostMessageW(0xFFFF, 0x0112, 0xF170, 2);
        }
    }
}

pub struct WinDelegate {
    mouse_mode: Arc<AtomicBool>,
    mouse_speed: AtomicU64,
    mouse_accel: AtomicBool,
    verbose_events: AtomicBool,
    display_asleep: AtomicBool,
    cursor: Mutex<(f64, f64)>,
    scroll_gen: Arc<AtomicU32>,
    scroll_pressed_active: AtomicBool,
    text_buffer: Mutex<String>,
}

impl WinDelegate {
    pub fn new(mouse_mode: Arc<AtomicBool>, speed: f64, accel: bool, verbose: bool) -> Self {
        unsafe { SetProcessDPIAware() };
        info!("[Windows] native input controller ready");
        Self {
            mouse_mode,
            mouse_speed: AtomicU64::new(speed.clamp(0.1, 10.0).to_bits()),
            mouse_accel: AtomicBool::new(accel),
            verbose_events: AtomicBool::new(verbose),
            display_asleep: AtomicBool::new(false),
            cursor: Mutex::new(cursor_pos()),
            scroll_gen: Arc::new(AtomicU32::new(0)),
            scroll_pressed_active: AtomicBool::new(false),
            text_buffer: Mutex::new(String::new()),
        }
    }

    pub fn reset_text_buffer(&self) {
        self.text_buffer.lock().unwrap().clear();
    }

    fn cancel_scroll(&self) {
        self.scroll_gen.fetch_add(1, Ordering::SeqCst);
        self.scroll_pressed_active.store(false, Ordering::SeqCst);
    }

    fn wake_display_if_asleep(&self) {
        if self.display_asleep.swap(false, Ordering::SeqCst) {
            set_monitor_power(true);
        }
    }

    fn start_scroll_hold(&self, delta: i32) {
        self.wake_display_if_asleep();
        self.scroll_pressed_active.store(true, Ordering::SeqCst);
        let gen = self.scroll_gen.fetch_add(1, Ordering::SeqCst) + 1;
        mouse_wheel(delta);

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let gen_ref = self.scroll_gen.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(280)).await;
            // Safety cap (~10s) prevents runaway scrolling if a release packet is lost.
            for _ in 0..110 {
                if gen_ref.load(Ordering::SeqCst) != gen {
                    return;
                }
                mouse_wheel(delta);
                tokio::time::sleep(Duration::from_millis(90)).await;
            }
        });
    }

    fn log(&self, msg: &str) {
        if self.verbose_events.load(Ordering::Relaxed) {
            info!("[Windows] {msg}");
        } else {
            debug!("[Windows] {msg}");
        }
    }

    fn mouse_move(&self, dx: f64, dy: f64) {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist == 0.0 {
            return;
        }
        let speed = f64::from_bits(self.mouse_speed.load(Ordering::Relaxed));
        // Same ballistics curve as the macOS controller.
        let accel = if !self.mouse_accel.load(Ordering::Relaxed) {
            1.6
        } else if dist < 1.5 {
            1.15
        } else if dist < 5.0 {
            1.15 + (dist - 1.5) * 0.35
        } else {
            2.375 + (dist - 5.0).min(10.0) * 0.22
        };
        let scale = accel * 1.6 * speed;
        let (l, t, r, b) = virtual_screen();
        let mut cur = self.cursor.lock().unwrap();
        cur.0 = (cur.0 + dx * scale).clamp(l, r);
        cur.1 = (cur.1 + dy * scale).clamp(t, b);
        unsafe { SetCursorPos(cur.0.round() as i32, cur.1.round() as i32) };
    }

    fn mouse_click(&self) {
        unsafe {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        }
    }
}

impl AtvDelegate for WinDelegate {
    fn on_button_state(&self, name: &str, pressed: bool) {
        if scroll_delta_for_button(name).is_none() {
            return;
        }
        if pressed {
            if let Some(delta) = scroll_delta_for_button(name) {
                self.start_scroll_hold(delta);
            }
        } else {
            // Stop hold-repeat; `on_button` right after consumes `scroll_pressed_active`.
            self.scroll_gen.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_button(&self, name: &str) {
        self.log(&format!("button: {name}"));
        if name == "power" || name == "sleep" || name == "wake" {
            let sleep = match name {
                "sleep" => true,
                "wake" => false,
                _ => !self.display_asleep.load(Ordering::SeqCst),
            };
            self.display_asleep.store(sleep, Ordering::SeqCst);
            return set_monitor_power(!sleep);
        }
        self.wake_display_if_asleep();
        if let Some(delta) = scroll_delta_for_button(name) {
            if !self.scroll_pressed_active.swap(false, Ordering::SeqCst) {
                // Synthetic callers (e.g. Web UI) trigger `on_button` without button-down.
                mouse_wheel(delta);
            }
            return;
        }
        match name {
            "select" if self.mouse_mode.load(Ordering::SeqCst) => self.mouse_click(),
            "select" => press_key(VK_RETURN),
            "siri" | "toggle_mode" => {
                let m = !self.mouse_mode.load(Ordering::SeqCst);
                self.mouse_mode.store(m, Ordering::SeqCst);
                info!("[Windows] input mode -> {}", if m { "mouse" } else { "direction" });
            }
            _ => match vk_for_button(name) {
                Some(vk) => press_key(vk),
                None => warn!("[Windows] no handler for button: {name}"),
            },
        }
    }

    fn on_text(&self, op: TextOp) {
        self.wake_display_if_asleep();
        self.log(&format!("text: {op:?}"));
        let (backspaces, insert) = {
            let mut buf = self.text_buffer.lock().unwrap();
            plan_text_edit(&mut buf, &op)
        };
        let inputs = build_text_inputs(backspaces, &insert);
        send_inputs(&inputs);
    }

    fn on_touch(&self, dx: f64, dy: f64, phase: TouchPhase) {
        if !self.mouse_mode.load(Ordering::SeqCst) {
            return;
        }
        match phase {
            // Re-sync with the real cursor in case the user moved the physical mouse.
            TouchPhase::Began => *self.cursor.lock().unwrap() = cursor_pos(),
            TouchPhase::Moved => self.mouse_move(dx, dy),
            TouchPhase::Ended => {}
        }
    }

    fn on_trackpad_mode_changed(&self, mode: TrackpadMode) {
        self.mouse_mode.store(mode.is_mouse(), Ordering::SeqCst);
        info!("[Windows] trackpad mode -> {}", mode.as_str());
    }

    fn on_mode_changed(&self, mouse_mode: bool) {
        self.mouse_mode.store(mouse_mode, Ordering::SeqCst);
    }

    fn on_touchpad_settings_changed(&self, speed: f64, acceleration: bool, verbose_events: bool) {
        let speed = speed.clamp(0.1, 10.0);
        self.mouse_speed.store(speed.to_bits(), Ordering::Relaxed);
        self.mouse_accel.store(acceleration, Ordering::Relaxed);
        self.verbose_events.store(verbose_events, Ordering::Relaxed);
        info!("[Windows] touchpad settings: speed={speed:.2}x accel={acceleration} verbose={verbose_events}");
    }

    fn get_touchpad_settings(&self) -> (f64, bool, bool) {
        (
            f64::from_bits(self.mouse_speed.load(Ordering::Relaxed)),
            self.mouse_accel.load(Ordering::Relaxed),
            self.verbose_events.load(Ordering::Relaxed),
        )
    }

    fn get_screen_size(&self) -> Option<(f64, f64)> {
        let (l, t, r, b) = virtual_screen();
        Some((r - l, b - t))
    }

    fn on_event(&self, kind: EventKind, detail: &str) {
        match kind {
            EventKind::MouseClick => self.mouse_click(),
            EventKind::ClientConnected => info!("[Windows] iPhone connected: {detail}"),
            EventKind::ClientDisconnected => {
                self.cancel_scroll();
                self.reset_text_buffer();
                info!("[Windows] iPhone disconnected: {detail}");
            }
            EventKind::SessionStopped => {
                self.cancel_scroll();
                self.reset_text_buffer();
                self.log(&format!("event: {kind:?} {detail}"));
            }
            _ => self.log(&format!("event: {kind:?} {detail}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_struct_matches_win32_abi() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<Input>(), 40);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<Input>(), 28);
    }

    #[test]
    fn scroll_button_mapping() {
        assert_eq!(scroll_delta_for_button("play_pause"), Some(WHEEL_DELTA));
        assert_eq!(scroll_delta_for_button("home"), Some(-WHEEL_DELTA));
        assert_eq!(scroll_delta_for_button("menu"), None);
        assert_eq!(vk_for_button("menu"), Some(0x1B));
        assert_eq!(vk_for_button("home"), None);
    }

    #[test]
    fn text_edit_plans_insert_delete_and_replace() {
        let mut buf = String::new();
        assert_eq!(
            plan_text_edit(&mut buf, &TextOp::Insert("你好".into())),
            (0, "你好".into())
        );
        assert_eq!(
            plan_text_edit(&mut buf, &TextOp::Replace("你好世界".into())),
            (0, "世界".into())
        );
        assert_eq!(
            plan_text_edit(&mut buf, &TextOp::Replace("你好天界".into())),
            (2, "天界".into())
        );
        assert_eq!(
            plan_text_edit(&mut buf, &TextOp::Delete(1)),
            (1, String::new())
        );
        assert_eq!(buf, "你好天");
    }

    #[test]
    fn build_text_inputs_handles_newline_and_surrogates() {
        let inputs = build_text_inputs(1, "a\r\nb😀");
        // 1 backspace (2) + 'a' (2) + '\r\n' (2) + 'b' (2) + '😀' 2 UTF-16 units (4) = 12 events
        assert_eq!(inputs.len(), 12);
        unsafe {
            assert_eq!(inputs[0].u.ki.w_vk, VK_BACK as u16);
            assert_eq!(inputs[4].u.ki.w_vk, VK_RETURN as u16);
            assert_eq!(inputs[8].u.ki.dw_flags, KEYEVENTF_UNICODE);
            assert_eq!(inputs[8].u.ki.w_scan, 0xD83D);
            assert_eq!(inputs[10].u.ki.w_scan, 0xDE00);
        }
    }
}
