//! Decode a Pi board revision or a device-tree compatible string.
//!
//! The running image keeps the drivers it was built with. Detection only
//! names the board when the revision agrees with that build. A mismatch,
//! or `detect` turned off, reports the build-time board.

pub const DETECT_DEFAULT: bool = true;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Virt,
    Pi4,
    Pi5,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub model: &'static str,
    pub family: Family,
    pub revision: u32,
    pub memory_mb: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub model: &'static str,
    pub source: &'static str,
    /// Driver set that stays linked into this image.
    pub drivers: &'static str,
}

pub fn family_name(family: Family) -> &'static str {
    match family {
        Family::Virt => "virt",
        Family::Pi4 => "pi4",
        Family::Pi5 => "pi5",
        Family::Other => "other",
    }
}

/// New-style revision (bit 23 set). `None` for an empty or old-style word.
pub fn decode_revision(word: u32) -> Option<Identity> {
    if word == 0 || word & (1 << 23) == 0 {
        return None;
    }
    let kind = (word >> 4) & 0xff;
    let memory = (word >> 20) & 0x7;
    let (model, family) = match kind {
        0x11 => ("pi4b", Family::Pi4),
        0x13 => ("pi400", Family::Pi4),
        0x14 | 0x15 => ("cm4", Family::Pi4),
        0x17 => ("pi5", Family::Pi5),
        _ => ("unknown", Family::Other),
    };
    Some(Identity {
        model,
        family,
        revision: word,
        memory_mb: memory_mb(memory),
    })
}

pub fn decode_compatible(blob: &[u8]) -> Option<Identity> {
    const TABLE: &[(&[u8], &str, Family)] = &[
        (b"raspberrypi,4-model-b", "pi4b", Family::Pi4),
        (b"raspberrypi,400", "pi400", Family::Pi4),
        (b"raspberrypi,4-compute-module", "cm4", Family::Pi4),
        (b"raspberrypi,5-model-b", "pi5", Family::Pi5),
        (b"linux,dummy-virt", "virt", Family::Virt),
        (b"arm,virt", "virt", Family::Virt),
    ];
    for (needle, model, family) in TABLE {
        if contains(blob, needle) {
            return Some(Identity {
                model,
                family: *family,
                revision: 0,
                memory_mb: 0,
            });
        }
    }
    None
}

pub fn choose(
    build: &'static str,
    revision: Option<Identity>,
    dtb: Option<Identity>,
    detect: bool,
) -> Choice {
    if !detect {
        return Choice {
            model: build,
            source: "build",
            drivers: build,
        };
    }
    if let Some(id) = revision {
        if family_name(id.family) == build {
            return Choice {
                model: id.model,
                source: "revision",
                drivers: build,
            };
        }
        return Choice {
            model: build,
            source: "build",
            drivers: build,
        };
    }
    if let Some(id) = dtb {
        if family_name(id.family) == build {
            return Choice {
                model: id.model,
                source: "dtb",
                drivers: build,
            };
        }
    }
    Choice {
        model: build,
        source: "build",
        drivers: build,
    }
}

fn memory_mb(code: u32) -> u32 {
    match code {
        0 => 256,
        1 => 512,
        2 => 1024,
        3 => 2048,
        4 => 4096,
        5 => 8192,
        6 => 16384,
        _ => 0,
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_style_codes_name_the_pi4_and_pi5() {
        let pi4 = decode_revision(0xc03112).unwrap();
        assert_eq!(pi4.model, "pi4b");
        assert_eq!(pi4.family, Family::Pi4);
        assert_eq!(pi4.memory_mb, 4096);
        let pi400 = decode_revision(0xa00130).unwrap();
        assert_eq!(pi400.model, "pi400");
        assert_eq!(pi400.family, Family::Pi4);
        let pi5 = decode_revision(0xc00170).unwrap();
        assert_eq!(pi5.model, "pi5");
        assert_eq!(pi5.family, Family::Pi5);
        assert_eq!(pi5.memory_mb, 4096);
        assert!(decode_revision(0).is_none());
        assert!(decode_revision(0x0010).is_none());
    }

    #[test]
    fn compatible_strings_and_the_detect_switch() {
        let dtb = decode_compatible(b"compatible\0raspberrypi,4-model-b\0").unwrap();
        assert_eq!(dtb.model, "pi4b");
        let matched = choose("pi4", decode_revision(0xc03112), None, true);
        assert_eq!(matched.model, "pi4b");
        assert_eq!(matched.source, "revision");
        assert_eq!(matched.drivers, "pi4");
        let wrong = choose("pi4", decode_revision(0xc00170), None, true);
        assert_eq!(wrong.model, "pi4");
        assert_eq!(wrong.source, "build");
        assert_eq!(wrong.drivers, "pi4");
        let off = choose("pi4", decode_revision(0xc03112), None, false);
        assert_eq!(off.model, "pi4");
        assert_eq!(off.source, "build");
        let from_dtb = choose("virt", None, decode_compatible(b"linux,dummy-virt"), true);
        assert_eq!(from_dtb.model, "virt");
        assert_eq!(from_dtb.source, "dtb");
        assert_eq!(from_dtb.drivers, "virt");
    }
}
