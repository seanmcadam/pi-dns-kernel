//! Host tests for the kernel fault-line layout. The kernel compiles the
//! same file. This crate does not link into the image.

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

#[path = "../../kernel/src/fault_log.rs"]
mod fault_log;
