//! Release string for this tree.
//!
//! Major stays 0 and minor stays 1. The patch number is the pull request
//! that introduced the build (`0.1.<pr>`). Bump it in this file when opening
//! a pull request. The kernel banner, the telnet log, `/status`, and the
//! firmware page all read [`VERSION`].

#![cfg_attr(not(test), no_std)]

macro_rules! release {
    () => {
        "0.1.64"
    };
}

/// `0.1.<pull request>`. Bump the literal in `release` when opening a PR.
pub const VERSION: &str = release!();

/// One boot line. The UART, HDMI console, and telnet preface print this.
pub const VERSION_LINE: &str = concat!("version ", release!());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_names_the_pull_request() {
        assert_eq!(VERSION, "0.1.64");
        assert_eq!(VERSION_LINE, "version 0.1.64");
        assert!(VERSION.starts_with("0.1."));
        let patch: u32 = VERSION[4..].parse().expect("patch");
        assert_eq!(patch, 64);
    }
}
