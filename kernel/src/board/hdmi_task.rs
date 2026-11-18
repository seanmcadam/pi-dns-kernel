// HDMI process pages. Included only by the Pi 4 and Pi 5 board modules.
// The virt image does not include this file, so it does not reserve the
// framebuffer window or link hdmi.bin.

const BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hdmi.bin"));
include!(concat!(env!("OUT_DIR"), "/hdmi_meta.rs"));

#[repr(C, align(4096))]
struct Mem {
    code: [u8; crate::board::HDMI_CODE_BYTES],
    data: [u8; crate::board::HDMI_DATA_BYTES],
    heap: [u8; crate::board::HDMI_HEAP_BYTES],
    stack: [u8; crate::board::HDMI_STACK_BYTES],
}

static mut MEM: Mem = Mem {
    code: [0; crate::board::HDMI_CODE_BYTES],
    data: [0; crate::board::HDMI_DATA_BYTES],
    heap: [0; crate::board::HDMI_HEAP_BYTES],
    stack: [0; crate::board::HDMI_STACK_BYTES],
};

pub fn image() -> crate::board::HdmiImage {
    unsafe {
        let code = addr_of_mut!(MEM.code).cast::<u8>();
        core::ptr::write_bytes(code, 0, crate::board::HDMI_CODE_BYTES);
        core::ptr::write_bytes(
            addr_of_mut!(MEM.data).cast::<u8>(),
            0,
            crate::board::HDMI_DATA_BYTES,
        );
        core::ptr::write_bytes(
            addr_of_mut!(MEM.heap).cast::<u8>(),
            0,
            crate::board::HDMI_HEAP_BYTES,
        );
        core::ptr::write_bytes(
            addr_of_mut!(MEM.stack).cast::<u8>(),
            0,
            crate::board::HDMI_STACK_BYTES,
        );
        let n = if BIN.len() < crate::board::HDMI_CODE_BYTES {
            BIN.len()
        } else {
            crate::board::HDMI_CODE_BYTES
        };
        core::ptr::copy_nonoverlapping(BIN.as_ptr(), code, n);
        if HDMI_DATA_LEN > 0
            && HDMI_DATA_OFF + HDMI_DATA_LEN <= BIN.len()
            && HDMI_DATA_PLACE + HDMI_DATA_LEN <= crate::board::HDMI_DATA_BYTES
        {
            core::ptr::copy_nonoverlapping(
                BIN.as_ptr().add(HDMI_DATA_OFF),
                addr_of_mut!(MEM.data).cast(),
                HDMI_DATA_LEN,
            );
        }
        crate::board::HdmiImage {
            code: addr_of!(MEM.code) as u64,
            bin_len: n,
            data: addr_of!(MEM.data) as u64,
            heap: addr_of!(MEM.heap) as u64,
            stack: addr_of!(MEM.stack) as u64,
        }
    }
}

pub fn code_ptr() -> *mut u8 {
    unsafe { addr_of_mut!(MEM.code).cast() }
}
