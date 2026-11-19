//! Which configured upstreams are in the lookup rotation.
//!
//! Every server starts up. A probe timeout, or a forwarded query timing
//! out, marks that server down and the next lookup skips it. A probe that
//! gets any reply marks it up again. When every server is down the
//! rotation is the full list, so a total outage still tries them instead
//! of failing every query. Client forward ids stay below `0x8000`. Probe
//! ids use that bit, so a probe reply cannot be returned to a client.

pub const MAX: usize = 8;
pub const LOG_GAP_MS: u64 = 5_000;
pub const PROBE_ID_BASE: u16 = 0x8000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub changed: bool,
    pub log: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    up: [bool; MAX],
    ip: [[u8; 4]; MAX],
    logged: [bool; MAX],
    log_ms: [u64; MAX],
    len: usize,
}

impl Health {
    pub const fn new(len: usize) -> Self {
        Self {
            up: [true; MAX],
            ip: [[0; 4]; MAX],
            logged: [false; MAX],
            log_ms: [0; MAX],
            len: if len > MAX { MAX } else { len },
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_up(&self, index: usize) -> bool {
        index < self.len && self.up[index]
    }

    /// Keep the up/down bit of an address that is still configured.
    /// A new address starts up.
    pub fn sync(&mut self, ips: &[[u8; 4]]) {
        let n = if ips.len() > MAX { MAX } else { ips.len() };
        let mut up = [true; MAX];
        let mut logged = [false; MAX];
        let mut log_ms = [0u64; MAX];
        let mut i = 0;
        while i < n {
            let mut j = 0;
            while j < self.len {
                if self.ip[j] == ips[i] {
                    up[i] = self.up[j];
                    logged[i] = self.logged[j];
                    log_ms[i] = self.log_ms[j];
                    break;
                }
                j += 1;
            }
            self.ip[i] = ips[i];
            i += 1;
        }
        self.up = up;
        self.logged = logged;
        self.log_ms = log_ms;
        self.len = n;
    }

    pub fn mark_down(&mut self, index: usize, now_ms: u64) -> Transition {
        if index >= self.len || !self.up[index] {
            return Transition {
                changed: false,
                log: false,
            };
        }
        self.up[index] = false;
        Transition {
            changed: true,
            log: self.should_log(index, now_ms),
        }
    }

    pub fn mark_up(&mut self, index: usize, now_ms: u64) -> Transition {
        if index >= self.len || self.up[index] {
            return Transition {
                changed: false,
                log: false,
            };
        }
        self.up[index] = true;
        Transition {
            changed: true,
            log: self.should_log(index, now_ms),
        }
    }

    pub fn mark_up_ip(&mut self, ip: [u8; 4], now_ms: u64) -> Transition {
        let mut i = 0;
        while i < self.len {
            if self.ip[i] == ip {
                return self.mark_up(i, now_ms);
            }
            i += 1;
        }
        Transition {
            changed: false,
            log: false,
        }
    }

    pub fn mark_down_ip(&mut self, ip: [u8; 4], now_ms: u64) -> Transition {
        let mut i = 0;
        while i < self.len {
            if self.ip[i] == ip {
                return self.mark_down(i, now_ms);
            }
            i += 1;
        }
        Transition {
            changed: false,
            log: false,
        }
    }

    /// Real upstream indices, in order. The full list when none are up.
    pub fn rotation(&self) -> Rotation {
        let mut idx = [0usize; MAX];
        let mut n = 0;
        let mut i = 0;
        while i < self.len {
            if self.up[i] {
                idx[n] = i;
                n += 1;
            }
            i += 1;
        }
        let all_down = n == 0 && self.len > 0;
        if all_down {
            i = 0;
            while i < self.len {
                idx[i] = i;
                i += 1;
            }
            n = self.len;
        }
        Rotation {
            idx,
            len: n,
            all_down,
        }
    }

    /// `count, ip0, up0, ...` with `up` as 0 or 1. Used by `/status`.
    pub fn pack(&self, ips: &[[u8; 4]], out: &mut [u8]) -> usize {
        let n = self.len.min(ips.len()).min(MAX);
        let need = 1 + n * 5;
        if out.len() < need {
            return 0;
        }
        out[0] = n as u8;
        let mut i = 0;
        while i < n {
            let off = 1 + i * 5;
            out[off..off + 4].copy_from_slice(&ips[i]);
            out[off + 4] = u8::from(self.is_up(i));
            i += 1;
        }
        need
    }

    fn should_log(&mut self, index: usize, now_ms: u64) -> bool {
        if self.logged[index] && now_ms.saturating_sub(self.log_ms[index]) < LOG_GAP_MS {
            return false;
        }
        self.logged[index] = true;
        self.log_ms[index] = now_ms;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rotation {
    idx: [usize; MAX],
    pub len: usize,
    pub all_down: bool,
}

impl Rotation {
    pub fn real(&self, pos: usize) -> Option<usize> {
        if pos < self.len {
            Some(self.idx[pos])
        } else {
            None
        }
    }

    pub fn position(&self, real: usize) -> Option<usize> {
        let mut i = 0;
        while i < self.len {
            if self.idx[i] == real {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    pub fn indices(&self) -> &[usize] {
        &self.idx[..self.len]
    }
}

/// One client query's walk. The snapshot stays put when a timeout marks a
/// server down, so this query still finishes the list it started with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walk {
    ips: [[u8; 4]; MAX],
    live_len: u8,
    origin: u8,
    tries: u8,
    pos: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub failed: [u8; 4],
    pub log_down: bool,
    pub next: Option<[u8; 4]>,
}

pub fn start_walk(health: &Health, ips: &[[u8; 4]], preferred_real: usize) -> Option<Walk> {
    let rot = health.rotation();
    if rot.len == 0 || ips.is_empty() {
        return None;
    }
    let pos = rot.position(preferred_real).unwrap_or(0);
    let choice = crate::failover::first(rot.len, pos)?;
    let mut stored = [[0u8; 4]; MAX];
    let mut i = 0;
    while i < rot.len {
        let real = rot.real(i)?;
        if real >= ips.len() {
            return None;
        }
        stored[i] = ips[real];
        i += 1;
    }
    Some(Walk {
        ips: stored,
        live_len: rot.len as u8,
        origin: choice.index as u8,
        tries: choice.tries,
        pos: choice.index as u8,
    })
}

impl Walk {
    pub fn current_ip(&self) -> Option<[u8; 4]> {
        let pos = self.pos as usize;
        if pos < self.live_len as usize {
            Some(self.ips[pos])
        } else {
            None
        }
    }

    /// Mark the server we just timed out down, then step to the next
    /// address in this snapshot. `next` is `None` when the snapshot is done.
    pub fn on_timeout(&mut self, health: &mut Health, now_ms: u64) -> Step {
        let pos = self.pos as usize;
        if pos >= self.live_len as usize {
            return Step {
                failed: [0; 4],
                log_down: false,
                next: None,
            };
        }
        let failed = self.ips[pos];
        let change = health.mark_down_ip(failed, now_ms);
        let Some(choice) = crate::failover::after_timeout(
            self.live_len as usize,
            self.origin as usize,
            self.tries,
        ) else {
            return Step {
                failed,
                log_down: change.log,
                next: None,
            };
        };
        self.pos = choice.index as u8;
        self.tries = choice.tries;
        Step {
            failed,
            log_down: change.log,
            next: Some(self.ips[choice.index]),
        }
    }
}

pub fn probe_id(index: usize) -> u16 {
    PROBE_ID_BASE | (index as u16 & 0x7)
}

pub fn probe_index(id: u16) -> usize {
    (id & 0x7) as usize
}

pub fn is_probe_id(id: u16) -> bool {
    id & PROBE_ID_BASE != 0
}

/// Forwarded client queries never use an id a probe would use.
pub fn next_client_id(next: &mut u16) -> u16 {
    let mut id = *next & 0x7fff;
    if id == 0 {
        id = 1;
    }
    let mut following = id.wrapping_add(1) & 0x7fff;
    if following == 0 {
        following = 1;
    }
    *next = following;
    id
}

/// A query for the root NS. Any reply, including REFUSED, means the
/// server is up. Only a timeout marks it down.
pub fn probe_query(id: u16) -> [u8; 17] {
    let mut packet = [0u8; 17];
    let bytes = id.to_be_bytes();
    packet[0] = bytes[0];
    packet[1] = bytes[1];
    packet[2] = 0x01;
    packet[5] = 0x01;
    packet[14] = 0x02;
    packet[16] = 0x01;
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three() -> (Health, [[u8; 4]; 3]) {
        let ips = [[1, 1, 1, 1], [2, 2, 2, 2], [3, 3, 3, 3]];
        let mut health = Health::new(0);
        health.sync(&ips);
        (health, ips)
    }

    #[test]
    fn down_server_leaves_rotation_until_a_probe_marks_it_up() {
        let (mut health, ips) = three();
        assert_eq!(health.rotation().indices(), &[0, 1, 2]);
        let down = health.mark_down(1, 1_000);
        assert!(down.changed && down.log);
        let again = health.mark_down(1, 1_500);
        assert!(!again.changed && !again.log);
        assert_eq!(health.rotation().indices(), &[0, 2]);

        let mut walk = start_walk(&health, &ips, 0).unwrap();
        assert_eq!(walk.current_ip(), Some([1, 1, 1, 1]));
        let step = walk.on_timeout(&mut health, 2_000);
        assert!(step.log_down);
        assert_eq!(step.next, Some([3, 3, 3, 3]));
        let done = walk.on_timeout(&mut health, 2_100);
        assert!(done.next.is_none());
        assert!(health.rotation().all_down);
        assert_eq!(health.rotation().indices(), &[0, 1, 2]);

        let up = health.mark_up(1, 3_000);
        assert!(up.changed);
        assert!(!up.log);
        assert_eq!(health.rotation().indices(), &[1]);
        let later = health.mark_down(1, 1_000 + LOG_GAP_MS);
        assert!(later.changed && later.log);
        let back = health.mark_up(1, 1_000 + LOG_GAP_MS * 2);
        assert!(back.changed && back.log);
        assert!(health.is_up(1));
    }

    #[test]
    fn every_server_down_falls_back_to_the_full_list() {
        let ips = [[8, 8, 8, 8], [1, 1, 1, 1]];
        let mut health = Health::new(0);
        health.sync(&ips);
        assert!(health.mark_down_ip([8, 8, 8, 8], 0).changed);
        assert!(health.mark_down_ip([1, 1, 1, 1], 0).changed);
        let rot = health.rotation();
        assert!(rot.all_down);
        assert_eq!(rot.indices(), &[0, 1]);
        let mut walk = start_walk(&health, &ips, 1).unwrap();
        assert_eq!(walk.current_ip(), Some([1, 1, 1, 1]));
        let step = walk.on_timeout(&mut health, 100);
        assert!(!step.log_down);
        assert_eq!(step.next, Some([8, 8, 8, 8]));
        assert!(walk.on_timeout(&mut health, 200).next.is_none());
        let fallback = start_walk(&health, &ips, 0).unwrap();
        assert_eq!(fallback.current_ip(), Some([8, 8, 8, 8]));
    }

    #[test]
    fn sync_keeps_a_down_server_when_the_list_is_reordered() {
        let mut health = Health::new(0);
        health.sync(&[[1, 1, 1, 1], [2, 2, 2, 2]]);
        assert!(health.mark_down_ip([2, 2, 2, 2], 0).log);
        health.sync(&[[2, 2, 2, 2], [1, 1, 1, 1], [3, 3, 3, 3]]);
        assert!(!health.is_up(0));
        assert!(health.is_up(1));
        assert!(health.is_up(2));
        assert_eq!(health.rotation().indices(), &[1, 2]);
        let mut packed = [0u8; 16];
        let n = health.pack(&[[2, 2, 2, 2], [1, 1, 1, 1], [3, 3, 3, 3]], &mut packed);
        assert_eq!(n, 16);
        assert_eq!(packed[0], 3);
        assert_eq!(&packed[1..5], &[2, 2, 2, 2]);
        assert_eq!(packed[5], 0);
        assert_eq!(packed[10], 1);
    }

    #[test]
    fn probe_ids_stay_out_of_the_client_space() {
        let mut next = 0x7fff;
        assert_eq!(next_client_id(&mut next), 0x7fff);
        assert_eq!(next, 1);
        assert_eq!(next_client_id(&mut next), 1);
        assert!(!is_probe_id(0x7fff));
        assert!(is_probe_id(probe_id(0)));
        assert_eq!(probe_id(3), 0x8003);
        assert_eq!(probe_index(0x8003), 3);
        let query = probe_query(probe_id(3));
        assert_eq!(u16::from_be_bytes([query[0], query[1]]), 0x8003);
        assert_eq!(query[5], 1);
        assert_eq!(query[12], 0);
        assert_eq!(&query[13..17], &[0, 2, 0, 1]);
    }

    #[test]
    fn empty_list_does_not_start_a_walk() {
        let health = Health::new(0);
        assert!(start_walk(&health, &[], 0).is_none());
        assert!(health.rotation().indices().is_empty());
        assert!(!health.rotation().all_down);
    }
}
