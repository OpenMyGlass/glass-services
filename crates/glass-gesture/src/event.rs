//! evdev-like input events and the codes the recognizer uses (from `linux/input-event-codes.h`).

/// One input event: type, code, value, and the kernel time in µs (`CLOCK_MONOTONIC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawEvent {
    pub time_us: u64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

/// Event types.
pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const ABS: u16 = 0x03;
}

/// `EV_SYN` codes.
pub mod syn {
    pub const REPORT: u16 = 0;
    pub const MT_REPORT: u16 = 2;
    pub const DROPPED: u16 = 3;
}

/// `EV_ABS` codes.
pub mod abs {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
    pub const MT_SLOT: u16 = 0x2f;
    pub const MT_TOUCH_MAJOR: u16 = 0x30;
    pub const MT_TOUCH_MINOR: u16 = 0x31;
    pub const MT_ORIENTATION: u16 = 0x34;
    pub const MT_POSITION_X: u16 = 0x35;
    pub const MT_POSITION_Y: u16 = 0x36;
    pub const MT_TOOL_TYPE: u16 = 0x37;
    pub const MT_TRACKING_ID: u16 = 0x39;
    pub const MT_PRESSURE: u16 = 0x3a;
}
