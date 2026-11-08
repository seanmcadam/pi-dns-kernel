// Shared HDMI state machine. Each board file sets BOARD and includes this.

const YIELD: u64 = 1;
const EXIT: u64 = 3;
const LOG_RECENT: u64 = 31;
const DEBUG_STATUS: u64 = 42;
const TIMER_TICKS: u64 = 51;
const TIMER_FREQ: u64 = 52;
const DISPLAY_IO: u64 = 55;

const SET_PHYSICAL: u32 = 0x0004_8003;
const SET_VIRTUAL: u32 = 0x0004_8004;
const SET_DEPTH: u32 = 0x0004_8005;
const SET_ORDER: u32 = 0x0004_8006;
const SET_OFFSET: u32 = 0x0004_8009;
const ALLOCATE: u32 = 0x0004_0001;
const GET_PITCH: u32 = 0x0004_0008;
const RELEASE: u32 = 0x0004_8001;
const GET_EDID: u32 = 0x0003_0020;

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    run();
}

fn run() -> ! {
    let mut live: Option<Fb> = None;
    loop {
        let hpd = sys(DISPLAY_IO, 0, 0, 0, 0, 0, 0);
        if hpd < 0 {
            drop_fb(&mut live);
            pause(1000);
            continue;
        }
        if hpd == 0 {
            drop_fb(&mut live);
            pause(500);
            continue;
        }
        if live.is_none() {
            live = connect();
        }
        if let Some(fb) = live.as_ref() {
            paint(fb);
        }
        pause(500);
    }
}

struct Fb {
    bus: u32,
    va: u64,
    bytes: u32,
    pitch: u32,
}

fn connect() -> Option<Fb> {
    let _edid = read_edid();
    let mut msg = [0u32; 40];
    let n = build_alloc(&mut msg);
    if mailbox(&mut msg[..n]) < 0 {
        return None;
    }
    let alloc = tag_words(&msg[..n], ALLOCATE)?;
    if alloc.len() < 2 || alloc[0] == 0 || alloc[1] == 0 {
        return None;
    }
    let pitch = tag_words(&msg[..n], GET_PITCH)
        .and_then(|words| words.first().copied())
        .filter(|pitch| *pitch >= display_console::STRIDE as u32)
        .unwrap_or(display_console::STRIDE as u32);
    let bus = alloc[0];
    let bytes = alloc[1];
    let va = sys(DISPLAY_IO, 2, bus as u64, bytes as u64, 0, 0, 0);
    if va <= 0 {
        release(bus);
        return None;
    }
    Some(Fb {
        bus,
        va: va as u64,
        bytes,
        pitch,
    })
}

fn drop_fb(live: &mut Option<Fb>) {
    let Some(fb) = live.take() else {
        return;
    };
    let _ = sys(DISPLAY_IO, 3, 0, 0, 0, 0, 0);
    release(fb.bus);
}

fn release(bus: u32) {
    let _ = bus;
    let mut msg = [0u32; 8];
    msg[0] = 7 * 4;
    msg[2] = RELEASE;
    msg[3] = 0;
    let _ = mailbox(&mut msg[..7]);
}

fn paint(fb: &Fb) {
    let mut log_buf = [0u8; 1024];
    let mut debug_buf = [0u8; 1200];
    let log_n = sys(LOG_RECENT, log_buf.as_mut_ptr() as u64, log_buf.len() as u64, 0, 0, 0, 0);
    let debug_n = sys(
        DEBUG_STATUS,
        debug_buf.as_mut_ptr() as u64,
        debug_buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    let rows = fb.bytes as usize / fb.pitch as usize;
    let bytes = rows * fb.pitch as usize;
    if bytes == 0 || fb.va == 0 {
        return;
    }
    unsafe {
        let buf = core::slice::from_raw_parts_mut(fb.va as *mut u8, bytes);
        display_console::draw(
            buf,
            fb.pitch as usize,
            BOARD,
            text_of(&log_buf, log_n),
            text_of(&debug_buf, debug_n),
        );
    }
}

fn text_of(buf: &[u8], n: i64) -> &str {
    if n <= 0 {
        return "";
    }
    let n = n as usize;
    let n = if n < buf.len() { n } else { buf.len() };
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

fn read_edid() -> Option<edid::Timing> {
    let mut msg = [0u32; 48];
    // size, code, tag, value bytes, request/response, 34 value words, end tag
    let words = 2 + 3 + 34 + 1;
    msg[0] = (words * 4) as u32;
    msg[2] = GET_EDID;
    msg[3] = 34 * 4;
    if mailbox(&mut msg[..words]) < 0 {
        return None;
    }
    let data = tag_words(&msg[..words], GET_EDID)?;
    if data.len() < 34 || data[1] != 0 {
        return None;
    }
    let mut block = [0u8; 128];
    let mut i = 0;
    while i < 32 {
        let word = data[2 + i];
        block[i * 4] = word as u8;
        block[i * 4 + 1] = (word >> 8) as u8;
        block[i * 4 + 2] = (word >> 16) as u8;
        block[i * 4 + 3] = (word >> 24) as u8;
        i += 1;
    }
    edid::preferred(&block)
}

fn build_alloc(msg: &mut [u32]) -> usize {
    msg[0] = 0;
    msg[1] = 0;
    let mut n = 2;
    n = put_tag(msg, n, SET_PHYSICAL, &[display_console::WIDTH as u32, display_console::HEIGHT as u32]);
    n = put_tag(msg, n, SET_VIRTUAL, &[display_console::WIDTH as u32, display_console::HEIGHT as u32]);
    n = put_tag(msg, n, SET_DEPTH, &[32]);
    n = put_tag(msg, n, SET_ORDER, &[0]);
    n = put_tag(msg, n, SET_OFFSET, &[0, 0]);
    n = put_tag(msg, n, ALLOCATE, &[4096, 0]);
    n = put_tag(msg, n, GET_PITCH, &[0]);
    msg[n] = 0;
    n += 1;
    msg[0] = (n * 4) as u32;
    n
}

fn put_tag(msg: &mut [u32], mut n: usize, tag: u32, values: &[u32]) -> usize {
    msg[n] = tag;
    msg[n + 1] = (values.len() * 4) as u32;
    msg[n + 2] = 0;
    let mut i = 0;
    while i < values.len() {
        msg[n + 3 + i] = values[i];
        i += 1;
    }
    n += 3 + values.len();
    n
}

fn tag_words(msg: &[u32], want: u32) -> Option<&[u32]> {
    if msg.len() < 3 {
        return None;
    }
    let mut i = 2;
    while i + 3 <= msg.len() {
        let tag = msg[i];
        if tag == 0 {
            return None;
        }
        let words = msg[i + 1] as usize / 4;
        if i + 3 + words > msg.len() {
            return None;
        }
        if tag == want {
            return Some(&msg[i + 3..i + 3 + words]);
        }
        i += 3 + words;
    }
    None
}

fn mailbox(words: &mut [u32]) -> i64 {
    let bytes = core::mem::size_of_val(words) as u64;
    sys(DISPLAY_IO, 1, words.as_mut_ptr() as u64, bytes, 0, 0, 0)
}

fn pause(ms: u64) {
    let freq = sys(TIMER_FREQ, 0, 0, 0, 0, 0, 0) as u64;
    if freq == 0 {
        yield_now();
        return;
    }
    let start = sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64;
    let need = freq / 1000 * ms;
    loop {
        let now = sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64;
        if now.wrapping_sub(start) >= need {
            return;
        }
        yield_now();
    }
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
        );
    }
    ret as i64
}

fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = sys(EXIT, 1, 0, 0, 0, 0, 0);
    loop {
        yield_now();
    }
}
