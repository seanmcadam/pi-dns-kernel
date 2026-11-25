//! Pi 4 HDMI console.
//!
//! The kernel only pokes the BCM2711 mailbox and the HDMI hotplug registers.
//! This process owns EDID, framebuffer allocation, and drawing. Hotplug is
//! polled. The register address and the connected bit are documented next to
//! the kernel read and have not been confirmed on a board.

#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("../../../user-common/start.s"));

const BOARD: &str = "pi4 hdmi";

include!("../../runner.rs");
