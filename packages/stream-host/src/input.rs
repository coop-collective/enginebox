use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

pub struct Permit {
    pub active: AtomicBool,
    pub control: bool,
    deadline: Mutex<Instant>,
}
impl Permit {
    pub fn new(control: bool, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            control,
            deadline: Mutex::new(Instant::now() + ttl),
        })
    }
    pub fn renew(&self, ttl: Duration) {
        let mut deadline = self.deadline.lock().unwrap();
        if Instant::now() >= *deadline {
            self.revoke();
            return;
        }
        *deadline = Instant::now() + ttl;
    }
    pub fn valid(&self) -> bool {
        if Instant::now() >= *self.deadline.lock().unwrap() {
            self.revoke();
        }
        self.active.load(Ordering::SeqCst)
    }
    pub fn revoke(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}
pub struct Input {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    keys: Vec<(u32, u8)>,
    pressed: HashSet<(bool, u8)>,
    budget: u32,
    window: Instant,
    pub count: Arc<AtomicU64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    #[serde(rename = "type")]
    kind: String,
    x: Option<f64>,
    y: Option<f64>,
    key: Option<String>,
    button: Option<u8>,
}
impl Input {
    pub fn connect(display: &str, width: u16, height: u16, count: Arc<AtomicU64>) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display))?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        let min = conn.setup().min_keycode;
        let max = conn.setup().max_keycode;
        let mapping = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let mut keys = Vec::new();
        for (i, syms) in mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .enumerate()
        {
            for s in syms {
                keys.push((*s, min + i as u8));
            }
        }
        Ok(Self {
            conn,
            root,
            width,
            height,
            keys,
            pressed: HashSet::new(),
            budget: 120,
            window: Instant::now(),
            count,
        })
    }
    pub fn apply(&mut self, bytes: &[u8], permit: &Permit) -> Result<()> {
        if !permit.valid() || !permit.control || bytes.len() > 1024 {
            bail!("input not authorized")
        }
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.budget = 120;
        }
        if self.budget == 0 {
            bail!("input rate exceeded")
        }
        self.budget -= 1;
        let event: Event = serde_json::from_slice(bytes)?;
        match event.kind.as_str() {
            "pointermove" | "pointerdown" | "pointerup" => {
                let x = event
                    .x
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v < f64::from(self.width))
                    .ok_or_else(|| anyhow::anyhow!("invalid x"))?;
                let y = event
                    .y
                    .filter(|v| v.is_finite() && *v >= 0.0 && *v < f64::from(self.height))
                    .ok_or_else(|| anyhow::anyhow!("invalid y"))?;
                self.conn.xtest_fake_input(
                    xproto::MOTION_NOTIFY_EVENT,
                    0,
                    0,
                    self.root,
                    x as i16,
                    y as i16,
                    0,
                )?;
                if event.kind != "pointermove" {
                    let b = event.button.unwrap_or(0);
                    if b > 2 {
                        bail!("unsupported pointer button")
                    }
                    let b = match b {
                        0 => 1,
                        1 => 2,
                        _ => 3,
                    };
                    let down = event.kind == "pointerdown";
                    self.change(false, b, down)?;
                }
            }
            "keydown" | "keyup" => {
                let sym = keysym(event.key.as_deref().unwrap_or(""))
                    .ok_or_else(|| anyhow::anyhow!("unsupported key"))?;
                let code = self
                    .keys
                    .iter()
                    .find_map(|(s, c)| (*s == sym).then_some(*c))
                    .ok_or_else(|| anyhow::anyhow!("key unavailable"))?;
                self.change(true, code, event.kind == "keydown")?;
            }
            _ => bail!("unsupported input event"),
        }
        self.conn.flush()?;
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn change(&mut self, key: bool, code: u8, down: bool) -> Result<()> {
        if down {
            self.pressed.insert((key, code));
        } else {
            self.pressed.remove(&(key, code));
        }
        let kind = match (key, down) {
            (true, true) => xproto::KEY_PRESS_EVENT,
            (true, false) => xproto::KEY_RELEASE_EVENT,
            (false, true) => xproto::BUTTON_PRESS_EVENT,
            (false, false) => xproto::BUTTON_RELEASE_EVENT,
        };
        self.conn
            .xtest_fake_input(kind, code, 0, self.root, 0, 0, 0)?;
        Ok(())
    }
    pub fn release_all(&mut self) {
        for (key, code) in std::mem::take(&mut self.pressed) {
            let _ = self.change(key, code, false);
        }
        let _ = self.conn.flush();
    }
    #[cfg(all(test, target_os = "linux"))]
    pub fn server_has_pressed_keys(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_keymap()?
            .reply()?
            .keys
            .iter()
            .any(|b| *b != 0))
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.release_all();
    }
}
fn keysym(key: &str) -> Option<u32> {
    Some(match key {
        "Space" | " " => 32,
        "Enter" => 0xff0d,
        "Escape" => 0xff1b,
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "Shift" | "ShiftLeft" => 0xffe1,
        "Control" | "ControlLeft" => 0xffe3,
        s if s.len() == 4 && s.starts_with("Key") && s.as_bytes()[3].is_ascii_uppercase() => {
            u32::from(s.as_bytes()[3].to_ascii_lowercase())
        }
        s if s.len() == 6 && s.starts_with("Digit") && s.as_bytes()[5].is_ascii_digit() => {
            u32::from(s.as_bytes()[5])
        }
        s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => {
            u32::from(s.as_bytes()[0].to_ascii_lowercase())
        }
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revoked_or_expired_permit_never_controls() {
        let p = Permit::new(true, Duration::from_secs(1));
        assert!(p.valid());
        p.revoke();
        assert!(!p.valid());
        let p = Permit::new(true, Duration::ZERO);
        assert!(!p.valid());
        p.renew(Duration::from_secs(60));
        assert!(!p.valid());
    }
    #[test]
    fn keys_are_explicit_not_shell_or_system_shortcuts() {
        assert_eq!(keysym("KeyA"), Some(97));
        assert_eq!(keysym("Meta"), None);
        assert_eq!(keysym("F12"), None);
    }
}
