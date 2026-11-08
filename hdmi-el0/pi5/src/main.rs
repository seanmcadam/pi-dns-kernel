//! Pi 5 HDMI console.
//!
//! RP1 HDMI and the VideoCore VII pixel pipeline are not programmed. The
//! kernel reports hotplug as disconnected, so this process stays idle. The
//! mailbox allocate and EDID path below still runs if a later kernel reports
//! a connected display. That mailbox address is an assumption.

#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("start.s"));

const BOARD: &str = "pi5 hdmi";

include!("../../runner.rs");
