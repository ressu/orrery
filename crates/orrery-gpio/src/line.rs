//! A GPIO line through the kernel's character device, `/dev/gpiochipN`, with uapi v2's line
//! request (`linux/gpio.h`): edge events, bias, active-low, and the kernel's own debounce.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use tokio::io::unix::AsyncFd;

use crate::pin::{Bias, Pin};

const FLAG_ACTIVE_LOW: u64 = 1 << 1;
const FLAG_INPUT: u64 = 1 << 2;
const FLAG_EDGE_RISING: u64 = 1 << 4;
const FLAG_EDGE_FALLING: u64 = 1 << 5;
const FLAG_BIAS_PULL_UP: u64 = 1 << 8;
const FLAG_BIAS_PULL_DOWN: u64 = 1 << 9;
const ATTR_DEBOUNCE: u32 = 3;
const EVENT_RISING_EDGE: u32 = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct LineAttribute {
    id: u32,
    padding: u32,
    /// A union in the kernel's struct: flags, values, or `debounce_period_us`, a u32.
    value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ConfigAttribute {
    attr: LineAttribute,
    mask: u64,
}

#[repr(C)]
struct LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [ConfigAttribute; 10],
}

#[repr(C)]
struct LineRequest {
    offsets: [u32; 64],
    consumer: [u8; 32],
    config: LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

#[repr(C)]
struct LineValues {
    bits: u64,
    mask: u64,
}

#[repr(C)]
#[derive(Default)]
struct LineEvent {
    timestamp_ns: u64,
    id: u32,
    offset: u32,
    seqno: u32,
    line_seqno: u32,
    padding: [u32; 6],
}

const _: () = assert!(size_of::<LineRequest>() == 592 && size_of::<LineEvent>() == 48);

const GET_LINE: rustix::ioctl::Opcode = rustix::ioctl::opcode::read_write::<LineRequest>(0xB4, 0x07);
const GET_VALUES: rustix::ioctl::Opcode = rustix::ioctl::opcode::read_write::<LineValues>(0xB4, 0x0E);

/// A change on the line: whether it's now active (pressed, with `!` taken into account), and
/// when, on the monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub active: bool,
    pub at: Duration,
}

pub struct Line {
    fd: AsyncFd<OwnedFd>,
}

impl Line {
    /// Requests the line as an input with edge events. `debounce` is the kernel's: a change
    /// counts once the line has stayed put this long.
    pub fn open(pin: &Pin, debounce: Duration) -> io::Result<Self> {
        let chip = std::fs::OpenOptions::new().read(true).write(true).open(format!("/dev/{}", pin.chip))?;
        let mut flags = FLAG_INPUT | FLAG_EDGE_RISING | FLAG_EDGE_FALLING;
        if pin.inverted {
            flags |= FLAG_ACTIVE_LOW;
        }
        flags |= match pin.bias {
            Bias::Default => 0,
            Bias::PullUp => FLAG_BIAS_PULL_UP,
            Bias::PullDown => FLAG_BIAS_PULL_DOWN,
        };
        let mut config = LineConfig { flags, num_attrs: 0, padding: [0; 5], attrs: [ConfigAttribute::default(); 10] };
        let us = u32::try_from(debounce.as_micros()).unwrap_or(u32::MAX);
        if us > 0 {
            // The u32 sits at the union's start.
            let value = if cfg!(target_endian = "little") { u64::from(us) } else { u64::from(us) << 32 };
            config.attrs[0] = ConfigAttribute { attr: LineAttribute { id: ATTR_DEBOUNCE, padding: 0, value }, mask: 1 };
            config.num_attrs = 1;
        }
        let mut request = LineRequest {
            offsets: [0; 64],
            consumer: [0; 32],
            config,
            num_lines: 1,
            event_buffer_size: 0,
            padding: [0; 5],
            fd: -1,
        };
        request.offsets[0] = pin.line;
        let consumer = b"orrery-gpio";
        request.consumer[..consumer.len()].copy_from_slice(consumer);
        // SAFETY: the opcode is GPIO_V2_GET_LINE_IOCTL, and `request` is laid out as the kernel's
        // struct gpio_v2_line_request (checked by size above); the kernel fills in `fd`.
        let ioctl = unsafe { rustix::ioctl::Updater::<GET_LINE, LineRequest>::new(&mut request) };
        // SAFETY: as above; the file is a GPIO chip.
        unsafe { rustix::ioctl::ioctl(&chip, ioctl) }?;
        // SAFETY: the kernel gave us this new file descriptor, and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(request.fd) };
        rustix::io::ioctl_fionbio(&fd, true)?;
        // SAFETY: the AsyncFd owns the descriptor, and nothing else closes or replaces it.
        let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }?;
        Ok(Self { fd })
    }

    /// Whether the line is active now.
    pub fn active(&self) -> io::Result<bool> {
        let mut values = LineValues { bits: 0, mask: 1 };
        // SAFETY: the opcode is GPIO_V2_LINE_GET_VALUES_IOCTL, and `values` is laid out as
        // struct gpio_v2_line_values.
        let ioctl = unsafe { rustix::ioctl::Updater::<GET_VALUES, LineValues>::new(&mut values) };
        // SAFETY: as above; the file is the line request's.
        unsafe { rustix::ioctl::ioctl(self.fd.get_ref(), ioctl) }?;
        Ok(values.bits & 1 == 1)
    }

    /// The next change.
    pub async fn edge(&self) -> io::Result<Edge> {
        loop {
            let mut guard = self.fd.readable().await?;
            let mut event = LineEvent::default();
            // SAFETY: LineEvent is plain integers, so any bytes the kernel writes are valid.
            let buf = unsafe { std::slice::from_raw_parts_mut((&raw mut event).cast::<u8>(), size_of::<LineEvent>()) };
            match guard.try_io(|fd| rustix::io::read(fd.get_ref(), &mut *buf).map_err(io::Error::from)) {
                Ok(Ok(n)) if n == size_of::<LineEvent>() => {
                    return Ok(Edge {
                        active: event.id == EVENT_RISING_EDGE,
                        at: Duration::from_nanos(event.timestamp_ns),
                    });
                }
                Ok(Ok(n)) => return Err(io::Error::other(format!("a short read of {n} bytes from the line"))),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    }
}

/// Now, on the clock the kernel stamps events with.
pub fn now() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}
