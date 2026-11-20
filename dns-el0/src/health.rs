//! Which configured upstreams are in the lookup rotation.
//!
//! Every server starts up. A probe timeout or a forwarded query timing
//! out counts as one failure. Three failures in a row, with no reply in
//! between, mark that server down and the next lookup skips it. Any
//! reply, probe or forwarded, clears the streak and marks the server up.
//! One queued timeout is not enough: under load a healthy server still
//! answers the next query. When every server is down the rotation is the
//! full list, so a total outage still tries them instead of failing every
//! query. Probes stay off until the interface has an address. Client
//! forward ids stay below `0x8000`. Probe ids use that bit, so a probe
//! reply cannot be returned to a client.

pub const MAX: usize = 8;
pub const LOG_GAP_MS: u64 = 5_000;
pub const PROBE_ID_BASE: u16 = 0x8000;
/// Consecutive misses before a server leaves the rotation.
pub const FAIL_LIMIT: u8 = 3;
/// `health.invalid` A, a short NXDOMAIN or REFUSED. A root NS answer is
/// hundreds of bytes and was dropped on the shared upstream socket.
pub const PROBE_LEN: usize = 32;

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
    log_up: [bool; MAX],
    log_ms: [u64; MAX],
    fail: [u8; MAX],
    len: usize,
}

impl Health {
    pub const fn new(len: usize) -> Self {
        Self {
            up: [true; MAX],
            ip: [[0; 4]; MAX],
            logged: [false; MAX],
            log_up: [false; MAX],
            log_ms: [0; MAX],
            fail: [0; MAX],
            len: if len > MAX { MAX } else { len },
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_up(&self, index: usize) -> bool {
        index < self.len && self.up[index]
    }

    /// Keep the up/down bit and failure streak of an address that is
    /// still configured. A new address starts up. The old table is
    /// matched by copy: writing each new address into `self.ip` during
    /// the search made a later slot miss its old entry and come back up.
    pub fn sync(&mut self, ips: &[[u8; 4]]) {
        let n = if ips.len() > MAX { MAX } else { ips.len() };
        let old_len = self.len;
        let old_ip = self.ip;
        let old_up = self.up;
        let old_logged = self.logged;
        let old_log_up = self.log_up;
        let old_log_ms = self.log_ms;
        let old_fail = self.fail;
        let mut up = [true; MAX];
        let mut logged = [false; MAX];
        let mut log_up = [false; MAX];
        let mut log_ms = [0u64; MAX];
        let mut fail = [0u8; MAX];
        let mut ip = [[0u8; 4]; MAX];
        let mut i = 0;
        while i < n {
            let mut j = 0;
            while j < old_len {
                if old_ip[j] == ips[i] {
                    up[i] = old_up[j];
                    logged[i] = old_logged[j];
                    log_up[i] = old_log_up[j];
                    log_ms[i] = old_log_ms[j];
                    fail[i] = old_fail[j];
                    break;
                }
                j += 1;
            }
            ip[i] = ips[i];
            i += 1;
        }
        self.ip = ip;
        self.up = up;
        self.logged = logged;
        self.log_up = log_up;
        self.log_ms = log_ms;
        self.fail = fail;
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
            log: self.should_log(index, now_ms, false),
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
            log: self.should_log(index, now_ms, true),
        }
    }

    /// One miss. The server stays up until `FAIL_LIMIT` misses in a row.
    pub fn note_failure(&mut self, ip: [u8; 4], now_ms: u64) -> Transition {
        let Some(index) = self.find(ip) else {
            return Transition {
                changed: false,
                log: false,
            };
        };
        if self.fail[index] < FAIL_LIMIT {
            self.fail[index] += 1;
        }
        if self.fail[index] >= FAIL_LIMIT {
            self.mark_down(index, now_ms)
        } else {
            Transition {
                changed: false,
                log: false,
            }
        }
    }

    /// Any reply from this server. Clears the miss streak and marks it up.
    pub fn note_success(&mut self, ip: [u8; 4], now_ms: u64) -> Transition {
        let Some(index) = self.find(ip) else {
            return Transition {
                changed: false,
                log: false,
            };
        };
        self.fail[index] = 0;
        self.mark_up(index, now_ms)
    }

    fn find(&self, ip: [u8; 4]) -> Option<usize> {
        let mut i = 0;
        while i < self.len {
            if self.ip[i] == ip {
                return Some(i);
            }
            i += 1;
        }
        None
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

    fn should_log(&mut self, index: usize, now_ms: u64, now_up: bool) -> bool {
        // A repeat of the same edge inside the gap stays quiet. The
        // opposite edge is the line operators look for, so a recovery
        // is logged even when the down line was a few seconds ago.
        if self.logged[index]
            && self.log_up[index] == now_up
            && now_ms.saturating_sub(self.log_ms[index]) < LOG_GAP_MS
        {
            return false;
        }
        self.logged[index] = true;
        self.log_up[index] = now_up;
        self.log_ms[index] = now_ms;
        true
    }
}

/// Probes need a route. Until the interface has an address they only
/// time out and condemn servers that are about to be replaced.
pub fn probes_allowed(network_ready: bool) -> bool {
    network_ready
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

    /// Count one timeout against the server we just asked, then step to
    /// the next address in this snapshot. The server leaves the rotation
    /// only after `FAIL_LIMIT` misses. `next` is `None` when the snapshot
    /// is done.
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
        let change = health.note_failure(failed, now_ms);
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

/// A datagram the forward path may match. Queries, truncated headers, and
/// packets that did not come from port 53 are not replies.
pub fn is_dns_response(packet: &[u8], from_port: u16) -> bool {
    packet.len() >= 12 && from_port == 53 && packet[2] & 0x80 != 0
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

/// A recursive A query for `health.invalid`. Any reply, including
/// NXDOMAIN or REFUSED, means the server is up. The name is reserved, so
/// the answer is a short packet instead of a root-NS referral.
pub fn probe_query(id: u16) -> [u8; PROBE_LEN] {
    let mut packet = [0u8; PROBE_LEN];
    let bytes = id.to_be_bytes();
    packet[0] = bytes[0];
    packet[1] = bytes[1];
    packet[2] = 0x01;
    packet[5] = 0x01;
    packet[12] = 6;
    packet[13..19].copy_from_slice(b"health");
    packet[19] = 7;
    packet[20..27].copy_from_slice(b"invalid");
    packet[29] = 0x01;
    packet[31] = 0x01;
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
    fn down_server_leaves_rotation_until_a_reply_marks_it_up() {
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
        assert!(!step.log_down);
        assert!(health.is_up(0));
        assert_eq!(step.next, Some([3, 3, 3, 3]));
        assert_eq!(health.rotation().indices(), &[0, 2]);

        let up = health.mark_up(1, 3_000);
        assert!(up.changed && up.log);
        assert_eq!(health.rotation().indices(), &[0, 1, 2]);
        let later = health.mark_down(1, 1_000 + LOG_GAP_MS);
        assert!(later.changed && later.log);
        let back = health.mark_up(1, 1_000 + LOG_GAP_MS * 2);
        assert!(back.changed && back.log);
        assert!(health.is_up(1));
    }

    #[test]
    fn one_forward_timeout_does_not_mark_a_healthy_server_down() {
        let (mut health, ips) = three();
        let mut walk = start_walk(&health, &ips, 0).unwrap();
        let step = walk.on_timeout(&mut health, 1_000);
        assert!(!step.log_down);
        assert!(health.is_up(0));
        assert_eq!(health.rotation().indices(), &[0, 1, 2]);
        assert_eq!(step.next, Some([2, 2, 2, 2]));
        assert!(!health.note_failure(ips[0], 1_500).changed);
        assert!(health.is_up(0));
        let down = health.note_failure(ips[0], 2_000);
        assert!(down.changed && down.log);
        assert!(!health.is_up(0));
        assert_eq!(health.rotation().indices(), &[1, 2]);
    }

    #[test]
    fn a_forwarded_reply_marks_the_server_up_and_clears_the_streak() {
        let (mut health, ips) = three();
        assert!(!health.note_failure(ips[1], 1_000).changed);
        assert!(!health.note_failure(ips[1], 2_000).changed);
        let down = health.note_failure(ips[1], 3_000);
        assert!(down.changed && down.log);
        assert!(!health.is_up(1));
        let up = health.note_success(ips[1], 3_100);
        assert!(up.changed && up.log);
        assert!(health.is_up(1));
        assert_eq!(health.rotation().indices(), &[0, 1, 2]);
        assert!(!health.note_failure(ips[1], 4_000).changed);
        assert!(!health.note_failure(ips[1], 5_000).changed);
        assert!(health.is_up(1));
        assert!(!health.note_success(ips[1], 6_000).changed);
    }

    #[test]
    fn probes_wait_until_the_network_is_configured() {
        assert!(!probes_allowed(false));
        assert!(probes_allowed(true));
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

        // Writing the new address into the table mid-search used to hide
        // the old address, so a down server that moved later came back up.
        let mut swapped = Health::new(0);
        swapped.sync(&[[1, 1, 1, 1], [2, 2, 2, 2]]);
        assert!(swapped.mark_down(0, 0).changed);
        assert!(!swapped.note_failure([2, 2, 2, 2], 10).changed);
        assert!(!swapped.note_failure([2, 2, 2, 2], 20).changed);
        swapped.sync(&[[2, 2, 2, 2], [1, 1, 1, 1]]);
        assert!(swapped.is_up(0));
        assert!(!swapped.is_up(1));
        assert!(swapped.note_failure([2, 2, 2, 2], 30).changed);
        assert!(!swapped.is_up(0));
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
        assert_eq!(query.len(), PROBE_LEN);
        assert_eq!(u16::from_be_bytes([query[0], query[1]]), 0x8003);
        assert_eq!(query[5], 1);
        assert_eq!(&query[12..20], b"\x06health\x07");
        assert_eq!(&query[20..28], b"invalid\x00");
        assert_eq!(&query[28..32], &[0, 1, 0, 1]);
        let mut response = query;
        assert!(!is_dns_response(&response, 53));
        response[2] |= 0x80;
        assert!(is_dns_response(&response, 53));
        assert!(!is_dns_response(&response, 53535));
        assert!(!is_dns_response(&response[..11], 53));
    }

    #[test]
    fn empty_list_does_not_start_a_walk() {
        let health = Health::new(0);
        assert!(start_walk(&health, &[], 0).is_none());
        assert!(health.rotation().indices().is_empty());
        assert!(!health.rotation().all_down);
    }
}
