//! Release string for this tree.
//!
//! Major stays 0 and minor stays 1. The patch number is the pull request
//! that introduced the build (`0.1.<pr>`). Bump it in this file when opening
//! a pull request. The kernel banner, the telnet log, `/status`, and the
//! firmware page all read [`VERSION`].

#![cfg_attr(not(test), no_std)]

macro_rules! release {
    () => {
        "0.1.81"
    };
}

/// `0.1.<pull request>`. Bump the literal in `release` when opening a PR.
pub const VERSION: &str = release!();

/// One boot line. The UART, HDMI console, and telnet preface print this.
pub const VERSION_LINE: &str = concat!("version ", release!());

/// Bytes matched against the tail of `KERNEL8.IMG` and `KERNEL8.NEW`.
/// The linker keeps this at the end of the image, so one short read can
/// tell which file is the kernel that is running.
pub const BOOT_ID_STR: &str = concat!("pi4dns-boot-id:", release!());

#[used]
#[link_section = ".bootid"]
pub static BOOT_ID: [u8; 32] = {
    let bytes = BOOT_ID_STR.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < bytes.len() && i < out.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_names_the_pull_request() {
        assert_eq!(VERSION, "0.1.81");
        assert_eq!(VERSION_LINE, "version 0.1.81");
        assert!(VERSION.starts_with("0.1."));
        let patch: u32 = VERSION[4..].parse().expect("patch");
        assert_eq!(patch, 81);
        assert!(BOOT_ID_STR.ends_with(VERSION));
        assert!(BOOT_ID.starts_with(BOOT_ID_STR.as_bytes()));
        assert_eq!(BOOT_ID[BOOT_ID_STR.len()], 0);
    }
}
