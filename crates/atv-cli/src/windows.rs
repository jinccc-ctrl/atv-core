//! Native Windows controller: translates Apple TV remote buttons and touch gestures
//! into Win32 keyboard/mouse input, media/volume keys, and monitor sleep/wake.
//!
//! Uses a handful of `user32` calls directly (no extra crates); no special
//! permission is required on Windows (unlike macOS Accessibility).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
const VK_MEDIA_NEXT: u8 = 0xB0;
const VK_MEDIA_PREV: u8 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u8 = 0xB3;

/// Windows virtual-key codes for simple remote buttons.
/// `volume_up` and `volume_down` are mapped to vertical mouse-wheel scrolling.
fn vk_for_button(name: &str) -> Option<u8> {
    Some(match name {
        "up" => 0x26,
        "down" => 0x28,
        "left" | "rewind" => 0x25,
        "right" | "fast_forward" => 0x27,
        "menu" => 0x1B,                         // Escape
        "home" => 0x24,                         // Home
        "channel_up" | "page_up" => 0x21,       // Page Up
        "channel_down" | "page_down" => 0x22,   // Page Down
        "guide" => 0x09,                        // Tab
        "play" | "pause" => VK_MEDIA_PLAY_PAUSE,
        "next" | "next_track" => VK_MEDIA_NEXT,
        "prev" | "previous" | "prev_track" | "previous_track" => VK_MEDIA_PREV,
        "mute" => VK_VOLUME_MUTE,
        _ => return None,
    })
}

fn scroll_delta_for_button(name: &str) -> Option<i32> {
    match name {
        "volume_up" => Some(WHEEL_DELTA),
        "volume_down" => Some(-WHEEL_DELTA),
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

#[derive(Default)]
struct TextTracker {
    buffer: String,
    last_edit_at: Option<Instant>,
    last_replace: Option<(String, Instant)>,
    last_insert: Option<(String, Instant)>,
    stale_after_reset: Option<(String, Instant)>,
}

impl TextTracker {
    fn reset(&mut self) {
        self.buffer.clear();
        self.last_edit_at = None;
        self.last_replace = None;
        self.last_insert = None;
        self.stale_after_reset = None;
    }

    fn reset_after_submit(&mut self, now: Instant) {
        let prev = std::mem::take(&mut self.buffer);
        self.last_edit_at = None;
        self.last_replace = None;
        self.last_insert = None;
        self.stale_after_reset = (!prev.is_empty()).then_some((prev, now));
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Update tracked remote input buffer and return `(backspaces, text_to_insert)`.
    /// Deduplicates redundant `Replace`/`Insert` pairs and immediate multi-character
    /// IME/dictation repeats (e.g. "你好" -> "你好你好").
    fn plan(&mut self, op: &TextOp, now: Instant) -> (u32, String) {
        match op {
            TextOp::Insert(text) => {
                if text.is_empty() {
                    return (0, String::new());
                }
                // The phone sometimes reports the whole field contents instead of
                // a delta (`insertionText` is cumulative after IME candidates and
                // dictation), and it retries an edit it believes was not applied.
                // Reconcile with what we already typed instead of appending.
                if !self.buffer.is_empty() {
                    if self.buffer == *text {
                        return (0, String::new());
                    }
                    if let Some(delta) = text.strip_prefix(self.buffer.as_str()) {
                        let delta = delta.to_string();
                        self.stale_after_reset = None;
                        self.buffer = text.clone();
                        self.last_edit_at = Some(now);
                        self.last_replace = None;
                        if delta.is_empty() {
                            self.last_insert = None;
                            return (0, String::new());
                        }
                        self.last_insert = Some((delta.clone(), now));
                        return (0, delta);
                    }
                    if let Some(dropped) = self.buffer.strip_prefix(text.as_str()) {
                        let backspaces = u32::try_from(dropped.chars().count()).unwrap_or(u32::MAX);
                        self.stale_after_reset = None;
                        self.buffer = text.clone();
                        self.last_edit_at = Some(now);
                        self.last_insert = None;
                        self.last_replace = None;
                        return (backspaces, String::new());
                    }
                }
                if let Some((ref replaced, t)) = self.last_replace {
                    if now.duration_since(t) <= Duration::from_millis(300)
                        && self.buffer == *replaced
                        && replaced.ends_with(text)
                    {
                        return (0, String::new());
                    }
                }
                if text.chars().count() >= 2 {
                    let is_dup_insert = self.last_insert.as_ref().is_some_and(|(prev, t)| {
                        prev == text && now.duration_since(*t) <= Duration::from_millis(250)
                    });
                    let already_committed_suffix = self.buffer.ends_with(text)
                        && self
                            .last_edit_at
                            .is_some_and(|t| now.duration_since(t) <= Duration::from_millis(250));
                    if is_dup_insert || already_committed_suffix {
                        return (0, String::new());
                    }
                }
                self.stale_after_reset = None;
                self.buffer.push_str(text);
                self.last_edit_at = Some(now);
                self.last_insert = Some((text.clone(), now));
                self.last_replace = None;
                (0, text.clone())
            }
            TextOp::Delete(count) => {
                self.stale_after_reset = None;
                for _ in 0..*count {
                    if self.buffer.pop().is_none() {
                        break;
                    }
                }
                self.last_edit_at = Some(now);
                self.last_insert = None;
                self.last_replace = None;
                (*count, String::new())
            }
            TextOp::Replace(target) => {
                if self.buffer.is_empty() {
                    if let Some((ref prev, t)) = self.stale_after_reset {
                        if now.duration_since(t) <= Duration::from_millis(400) {
                            let prev_trimmed = prev.trim_end_matches(['\r', '\n']);
                            let target_trimmed = target.trim_end_matches(['\r', '\n']);
                            if !target_trimmed.is_empty() && prev_trimmed == target_trimmed {
                                return (0, String::new());
                            }
                        }
                    }
                }
                self.stale_after_reset = None;
                let common_chars = self
                    .buffer
                    .chars()
                    .zip(target.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                let old_chars = self.buffer.chars().count();
                let backspaces = u32::try_from(old_chars - common_chars).unwrap_or(u32::MAX);
                let insert: String = target.chars().skip(common_chars).collect();
                self.buffer = target.clone();
                self.last_edit_at = Some(now);
                self.last_replace = Some((target.clone(), now));
                self.last_insert = None;
                (backspaces, insert)
            }
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
        for &mut unit in ch.encode_utf16(&mut utf16) {
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
    text_tracker: Mutex<TextTracker>,
    text_input: Mutex<Option<Arc<atv_core::text_input::TextInputHub>>>,
    /// (last click time, click count) for play/pause multi-click detection.
    play_clicks: Arc<Mutex<(Instant, u32)>>,
    play_gen: Arc<AtomicU32>,
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
            text_tracker: Mutex::new(TextTracker::default()),
            text_input: Mutex::new(None),
            play_clicks: Arc::new(Mutex::new((Instant::now() - Duration::from_secs(10), 0))),
            play_gen: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn set_text_input(&self, hub: Option<Arc<atv_core::text_input::TextInputHub>>) {
        *self.text_input.lock().unwrap() = hub;
    }

    pub fn reset_text_buffer(&self) {
        self.text_tracker.lock().unwrap().reset();
    }

    fn reset_text_session_at(&self, now: Instant) {
        self.text_tracker.lock().unwrap().reset_after_submit(now);
        if let Some(hub) = self.text_input.lock().unwrap().clone() {
            hub.reset_session();
        }
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

    /// Single click: play/pause, double: next track, triple: previous track.
    fn handle_play_pause(&self) {
        let count = {
            let mut s = self.play_clicks.lock().unwrap();
            let now = Instant::now();
            s.1 = if now.duration_since(s.0) <= Duration::from_millis(380) { s.1 + 1 } else { 1 };
            s.0 = now;
            s.1
        };
        let gen = self.play_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= 3 {
            self.play_clicks.lock().unwrap().1 = 0;
            info!("[Windows] media previous track (triple-click)");
            return press_key(VK_MEDIA_PREV);
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (clicks, gen_ref) = (self.play_clicks.clone(), self.play_gen.clone());
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if gen_ref.load(Ordering::SeqCst) != gen {
                return;
            }
            let n = std::mem::take(&mut clicks.lock().unwrap().1);
            match n {
                1 => press_key(VK_MEDIA_PLAY_PAUSE),
                2 => {
                    info!("[Windows] media next track (double-click)");
                    press_key(VK_MEDIA_NEXT)
                }
                _ => {}
            }
        });
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
            "select" if self.mouse_mode.load(Ordering::SeqCst) => {
                self.mouse_click();
                if !self.text_tracker.lock().unwrap().is_empty() {
                    self.reset_text_session_at(Instant::now());
                }
            }
            "select" => {
                press_key(VK_RETURN);
                if !self.text_tracker.lock().unwrap().is_empty() {
                    self.reset_text_session_at(Instant::now());
                }
            }
            "play_pause" => self.handle_play_pause(),
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
        let now = Instant::now();
        let (backspaces, insert) = {
            let mut tracker = self.text_tracker.lock().unwrap();
            tracker.plan(&op, now)
        };
        if backspaces == 0 && insert.is_empty() {
            return;
        }
        let submitted = insert.contains('\n') || insert.contains('\r');
        let inputs = build_text_inputs(backspaces, &insert);
        send_inputs(&inputs);
        if submitted {
            self.reset_text_session_at(now);
        }
    }

    fn on_text_stopped(&self) {
        self.reset_text_buffer();
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
            EventKind::MouseClick => {
                self.mouse_click();
                if !self.text_tracker.lock().unwrap().is_empty() {
                    self.reset_text_session_at(Instant::now());
                }
            }
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
        assert_eq!(scroll_delta_for_button("volume_up"), Some(WHEEL_DELTA));
        assert_eq!(scroll_delta_for_button("volume_down"), Some(-WHEEL_DELTA));
        assert_eq!(scroll_delta_for_button("play_pause"), None);
        assert_eq!(scroll_delta_for_button("home"), None);
        assert_eq!(scroll_delta_for_button("menu"), None);
        assert_eq!(vk_for_button("menu"), Some(0x1B));
        assert_eq!(vk_for_button("home"), Some(0x24));
        assert_eq!(vk_for_button("volume_up"), None);
        assert_eq!(vk_for_button("volume_down"), None);
    }

    #[test]
    fn text_edit_plans_insert_delete_and_replace() {
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好".into()), t0),
            (0, "你好".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Replace("你好世界".into()), t0 + Duration::from_millis(50)),
            (0, "世界".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Replace("你好天界".into()), t0 + Duration::from_millis(100)),
            (2, "天界".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Delete(1), t0 + Duration::from_millis(150)),
            (1, String::new())
        );
        assert_eq!(tracker.buffer, "你好天");
    }

    #[test]
    fn text_edit_deduplicates_repeated_chinese_and_dictation_ops() {
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();

        // 1. Insert("你好") followed by identical Replace("你好") -> no duplicate output
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好".into()), t0),
            (0, "你好".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Replace("你好".into()), t0 + Duration::from_millis(30)),
            (0, String::new())
        );

        // 2. Replace("你好") immediately followed by redundant Insert("你好") -> suppressed
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好".into()), t0 + Duration::from_millis(60)),
            (0, String::new())
        );
        assert_eq!(tracker.buffer, "你好");

        // 3. Duplicate multi-char Insert("世界") within 250ms -> suppressed
        assert_eq!(
            tracker.plan(&TextOp::Insert("世界".into()), t0 + Duration::from_millis(400)),
            (0, "世界".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Insert("世界".into()), t0 + Duration::from_millis(450)),
            (0, String::new())
        );
        assert_eq!(tracker.buffer, "你好世界");

        // 4. Separate intentional multi-char input later (>250ms) still types normally
        assert_eq!(
            tracker.plan(&TextOp::Insert("世界".into()), t0 + Duration::from_millis(900)),
            (0, "世界".into())
        );
        assert_eq!(tracker.buffer, "你好世界世界");

        // 5. Rapid single-char taps ("a" + "a") are NOT suppressed
        assert_eq!(
            tracker.plan(&TextOp::Insert("a".into()), t0 + Duration::from_millis(950)),
            (0, "a".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Insert("a".into()), t0 + Duration::from_millis(970)),
            (0, "a".into())
        );
        assert_eq!(tracker.buffer, "你好世界世界aa");
    }

    #[test]
    fn text_edit_reconciles_cumulative_inserts_from_iphone() {
        // Captured from a real iPhone: the phone re-sends whole-field text
        // ("你好" x6, then "世界", then "你好世界" x3, then " abcdef", ...).
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();
        let plan = |t: &mut TextTracker, text: &str, ms: u64| {
            t.plan(&TextOp::Insert(text.into()), t0 + Duration::from_millis(ms))
        };
        assert_eq!(plan(&mut tracker, "你好", 0), (0, "你好".into()));
        assert_eq!(plan(&mut tracker, "你好", 5), (0, String::new()));
        assert_eq!(plan(&mut tracker, "你好", 3000), (0, String::new()));
        assert_eq!(plan(&mut tracker, "世界", 3100), (0, "世界".into()));
        assert_eq!(plan(&mut tracker, "你好世界", 3200), (0, String::new()));
        assert_eq!(plan(&mut tracker, " abcdef", 9000), (0, " abcdef".into()));
        assert_eq!(plan(&mut tracker, "你好世界 abcdef", 9100), (0, String::new()));
        assert_eq!(tracker.buffer, "你好世界 abcdef");
    }

    #[test]
    fn text_edit_trims_cumulative_assert_without_prior_delta() {
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好".into()), t0),
            (0, "你好".into())
        );
        // Phone jumps straight to the full text: only the new suffix is typed.
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好世界".into()), t0 + Duration::from_millis(1000)),
            (0, "世界".into())
        );
        // Phone reports a shorter text without deletionCount: backspace the tail.
        assert_eq!(
            tracker.plan(&TextOp::Insert("你好".into()), t0 + Duration::from_millis(2000)),
            (2, String::new())
        );
        assert_eq!(tracker.buffer, "你好");
    }

    #[test]
    fn text_edit_still_appends_real_deltas() {
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();
        assert_eq!(
            tracker.plan(&TextOp::Insert("abc".into()), t0),
            (0, "abc".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Insert("def".into()), t0 + Duration::from_millis(1000)),
            (0, "def".into())
        );
        assert_eq!(tracker.buffer, "abcdef");
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

    #[test]
    fn text_edit_does_not_accumulate_across_submit_or_session_reset() {
        let mut tracker = TextTracker::default();
        let t0 = Instant::now();

        // 1st input: "你好" + Return -> reset after submit
        assert_eq!(
            tracker.plan(&TextOp::Replace("你好".into()), t0),
            (0, "你好".into())
        );
        assert_eq!(
            tracker.plan(&TextOp::Insert("\n".into()), t0 + Duration::from_millis(50)),
            (0, "\n".into())
        );
        tracker.reset_after_submit(t0 + Duration::from_millis(50));
        assert!(tracker.is_empty());

        // Stale in-flight Replace("你好") within 400ms is dropped rather than re-typed
        assert_eq!(
            tracker.plan(&TextOp::Replace("你好".into()), t0 + Duration::from_millis(120)),
            (0, String::new())
        );

        // 2nd input: "哈哈" -> only "哈哈" is typed
        assert_eq!(
            tracker.plan(&TextOp::Replace("哈哈".into()), t0 + Duration::from_millis(500)),
            (0, "哈哈".into())
        );
        tracker.reset_after_submit(t0 + Duration::from_millis(600));

        // 3rd input: "哦哦" -> only "哦哦" is typed
        assert_eq!(
            tracker.plan(&TextOp::Replace("哦哦".into()), t0 + Duration::from_millis(1200)),
            (0, "哦哦".into())
        );
        assert_eq!(tracker.buffer, "哦哦");
    }
}
