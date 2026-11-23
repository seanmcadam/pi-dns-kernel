//! Host tests for the kernel fault-line layout. The kernel compiles the
//! same file. This crate does not link into the image.

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

#[path = "../../kernel/src/addr.rs"]
mod addr;

#[path = "../../kernel/src/fault_log.rs"]
mod fault_log;

#[path = "../../kernel/src/crash_rec.rs"]
mod crash_rec;

#[path = "../../kernel/src/dhcp_rx.rs"]
mod dhcp_rx;

#[path = "../../dns-el0/src/failover.rs"]
mod failover;

#[path = "../../dns-el0/src/health.rs"]
mod health;
