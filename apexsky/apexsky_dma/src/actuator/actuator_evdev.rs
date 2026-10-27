use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LAST_ACTION_LOG_MS: AtomicU64 = AtomicU64::new(0);

fn log_action(x: i16, y: i16) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_ACTION_LOG_MS.load(Ordering::Relaxed);
    if ms.saturating_sub(last) > 1000 {
        LAST_ACTION_LOG_MS.store(ms, Ordering::Relaxed);
        tracing::info!(x, y, "evdev aim action emitted");
    }
}

use super::{delta_to_mouse_move, AimActuator};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const EV_SYN_REPORT: u16 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct EvdevAimActuator {
    mouse: File,
}

impl EvdevAimActuator {
    pub fn connect(path: &str) -> anyhow::Result<Self> {
        if path.trim().is_empty() {
            anyhow::bail!("evdev mouse path is empty");
        }
        let mouse = OpenOptions::new().read(true).write(true).open(path)?;
        tracing::info!(path, "evdev mouse actuator connected");
        Ok(Self { mouse })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> io::Result<()> {
        let event = InputEvent {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&event as *const InputEvent).cast::<u8>(),
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.mouse.write_all(bytes).map_err(|e| {
            tracing::warn!(%e, type_, code, value, "evdev input write failed");
            e
        })
    }

    fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, EV_SYN_REPORT, 0)
    }

    fn move_relative(&mut self, x: i16, y: i16) -> io::Result<()> {
        if x != 0 {
            self.emit(EV_REL, REL_X, i32::from(x))?;
        }
        if y != 0 {
            self.emit(EV_REL, REL_Y, i32::from(y))?;
        }
        self.sync()?;
        log_action(x, y);
        Ok(())
    }

    fn left_button(&mut self, down: bool) -> io::Result<()> {
        self.emit(EV_KEY, BTN_LEFT, i32::from(down))?;
        self.sync()
    }
}

impl AimActuator for EvdevAimActuator {
    async fn perform(&mut self, action: super::AimbotAction) -> anyhow::Result<()> {
        if let Some(down) = action.force_attack {
            self.left_button(down)?;
        }
        if let Some(delta) = action.shift_angles {
            let (x, y) = delta_to_mouse_move(delta);
            self.move_relative(x, y)?;
        }
        Ok(())
    }
}

impl Drop for EvdevAimActuator {
    fn drop(&mut self) {
        let _ = self.mouse.as_raw_fd();
    }
}
