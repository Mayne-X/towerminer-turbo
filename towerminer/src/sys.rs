// SPDX-License-Identifier: Apache-2.0
//! Machine facts read from the kernel: cache sizes, SMT siblings, affinity,
//! huge pages, temperature. Nothing here guesses from a CPU model name.
use std::collections::BTreeSet;
use std::fs;

use crate::walk::PAD_BYTES;

const HUGE: usize = 2 << 20;
const PAGE: usize = 4096;

/// One region of `pads` contiguous 512 KiB pads, 2 MiB aligned and advised
/// for transparent huge pages. A pad set in 4 KiB pages misses the dTLB on
/// almost every read [MEASURED 2026-09-26: 2 pads/core, THP vs 4K = 21.5 vs
/// 13.9 kH/s on a 7900X; +21 % single-thread on an EPYC 7742; 2026-09-28:
/// -24 % on a 5950X thread].
///
/// The mapping carries one PROT_NONE page at each end, so the kernel can
/// never merge it with a neighbour: the VMA that holds `base` in
/// /proc/self/smaps is this region's and nobody else's (see [`Region::huge_kb`]).
pub struct Region {
    map: *mut u8,
    map_len: usize,
    base: *mut u8,
    /// Length of the advised (huge) span starting at `base`.
    span: usize,
    pub pads: Vec<*mut u64>,
}

unsafe impl Send for Region {}

impl Region {
    /// Allocated and first-touched by the calling thread, so its pages land on
    /// that thread's NUMA node.
    pub fn new(pads: usize, huge: bool) -> std::io::Result<Region> {
        unsafe {
            let span = (pads * PAD_BYTES).div_ceil(HUGE) * HUGE;
            // span + 2M of alignment slack, + one guard page at each end.
            let map_len = span + HUGE + 2 * PAGE;
            let p = libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            let map = p as *mut u8;
            if libc::mprotect(map as *mut _, PAGE, libc::PROT_NONE) != 0
                || libc::mprotect(map.add(map_len - PAGE) as *mut _, PAGE, libc::PROT_NONE) != 0
            {
                let e = std::io::Error::last_os_error();
                libc::munmap(p, map_len);
                return Err(e);
            }
            let first = map as usize + PAGE;
            let base = ((first + HUGE - 1) & !(HUGE - 1)) as *mut u8;
            debug_assert!(base as usize + span <= map as usize + map_len - PAGE);
            let advice = if huge { libc::MADV_HUGEPAGE } else { libc::MADV_NOHUGEPAGE };
            libc::madvise(base as *mut _, span, advice);
            std::ptr::write_bytes(base, 0, pads * PAD_BYTES);
            let v = (0..pads).map(|k| base.add(k * PAD_BYTES) as *mut u64).collect();
            Ok(Region { map, map_len, base, span, pads: v })
        }
    }

    /// AnonHugePages (KiB) of the VMA holding this region, read from
    /// /proc/self/smaps. `None` if smaps cannot be read.
    pub fn huge_kb(&self) -> Option<u64> {
        let s = fs::read_to_string("/proc/self/smaps").ok()?;
        smaps_anon_huge_kb(&s, self.base as usize)
    }

    /// What `huge_kb` reads when every pad sits in 2 MiB pages.
    pub fn expected_huge_kb(&self) -> u64 {
        (self.span / 1024) as u64
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.map as *mut _, self.map_len);
        }
    }
}

/// `AnonHugePages` of the smaps block whose `[start, end)` contains `addr`.
pub fn smaps_anon_huge_kb(smaps: &str, addr: usize) -> Option<u64> {
    let mut inside = false;
    for line in smaps.lines() {
        let head = line.split(' ').next().unwrap_or("");
        if let Some((a, b)) = head.split_once('-') {
            if let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) {
                if inside {
                    // Next VMA without an AnonHugePages line in ours.
                    return Some(0);
                }
                inside = a <= addr && addr < b;
                continue;
            }
        }
        if inside {
            if let Some(v) = line.strip_prefix("AnonHugePages:") {
                return v.split_whitespace().next()?.parse().ok();
            }
        }
    }
    if inside {
        Some(0)
    } else {
        None
    }
}

pub fn anon_huge_kb() -> u64 {
    fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("AnonHugePages:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

pub fn thp_mode() -> String {
    fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .map(|s| {
            s.split_whitespace()
                .find(|w| w.starts_with('['))
                .unwrap_or("?")
                .trim_matches(|c| c == '[' || c == ']')
                .to_string()
        })
        .unwrap_or_else(|_| "unavailable".into())
}

pub fn parse_list(s: &str) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(v) = part.trim().parse() {
            out.insert(v);
        }
    }
    out
}

pub fn allowed_cpus() -> BTreeSet<usize> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return (0..std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).collect();
        }
        (0..libc::CPU_SETSIZE as usize).filter(|&c| libc::CPU_ISSET(c, &set)).collect()
    }
}

pub fn pin_current(cpu: usize) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// Busy and total jiffies of `cpus` since boot, from /proc/stat. Busy = user
/// + nice + system + irq + softirq + steal; total adds idle + iowait.
pub fn cpu_ticks(cpus: &[usize]) -> Option<(u64, u64)> {
    cpu_ticks_in(&fs::read_to_string("/proc/stat").ok()?, cpus)
}

fn cpu_ticks_in(stat: &str, cpus: &[usize]) -> Option<(u64, u64)> {
    let (mut busy, mut total, mut seen) = (0u64, 0u64, 0usize);
    for l in stat.lines() {
        // "cpuN ..." only: the aggregate "cpu  ..." line is not a CPU.
        let Some(rest) = l.strip_prefix("cpu").filter(|r| r.starts_with(|c: char| c.is_ascii_digit())) else {
            continue;
        };
        let mut f = rest.split_whitespace();
        let Some(id) = f.next().and_then(|x| x.parse::<usize>().ok()) else { continue };
        if !cpus.contains(&id) {
            continue;
        }
        let v: Vec<u64> = f.take(8).map(|x| x.parse().unwrap_or(0)).collect();
        if v.len() < 8 {
            continue;
        }
        let b = v[0] + v[1] + v[2] + v[5] + v[6] + v[7];
        busy += b;
        total += b + v[3] + v[4];
        seen += 1;
    }
    (seen > 0).then_some((busy, total))
}

/// CPU time of this process, all threads (utime + stime), jiffies.
pub fn self_ticks() -> Option<u64> {
    let s = fs::read_to_string("/proc/self/stat").ok()?;
    let f: Vec<&str> = s.rsplit_once(')')?.1.split_whitespace().collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

fn cache_kib(cpu: usize, index: usize) -> Option<usize> {
    let s = fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{index}/size")).ok()?;
    let s = s.trim();
    if let Some(k) = s.strip_suffix('K') {
        k.parse().ok()
    } else if let Some(m) = s.strip_suffix('M') {
        m.parse::<usize>().ok().map(|m| m * 1024)
    } else {
        s.parse().ok()
    }
}

pub struct Topology {
    /// Physical cores: sibling groups restricted to the allowed CPUs, each
    /// ordered, groups ordered by their first CPU.
    pub cores: Vec<Vec<usize>>,
    pub l2_kib: usize,
    pub l3_kib: usize,
}

impl Topology {
    pub fn detect(allowed: &BTreeSet<usize>) -> Topology {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut seen = BTreeSet::new();
        for &c in allowed {
            if seen.contains(&c) {
                continue;
            }
            let sib = fs::read_to_string(format!(
                "/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list"
            ))
            .map(|s| parse_list(&s))
            .unwrap_or_else(|_| [c].into_iter().collect());
            let g: Vec<usize> = sib.into_iter().filter(|x| allowed.contains(x)).collect();
            seen.extend(g.iter().copied());
            groups.push(g);
        }
        let first = *allowed.iter().next().unwrap_or(&0);
        // index2 = unified L2 on every x86 part this targets; index3 = L3.
        Topology {
            cores: groups,
            l2_kib: cache_kib(first, 2).unwrap_or(512),
            l3_kib: cache_kib(first, 3).unwrap_or(0),
        }
    }
}

/// Other miners on this machine, as `pid:exe`. Reported, never touched.
pub fn other_miners() -> Vec<String> {
    let mut others = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return others };
    let me = std::process::id().to_string();
    for e in dir.flatten() {
        let pid = e.file_name().to_string_lossy().to_string();
        if pid == me || !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(cmd) = fs::read(e.path().join("cmdline")) {
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
            let first = cmd.split_whitespace().next().unwrap_or("");
            let exe = first.rsplit('/').next().unwrap_or("");
            if ["towerminer", "jetsam-miner", "veld-worker", "xmrig", "rplant"].iter().any(|m| exe.starts_with(m))
                || cmd.contains("--mode miner")
            {
                others.push(format!("{pid}:{exe}"));
            }
        }
    }
    others
}

/// SHA-256 (FIPS 180-4) of a byte string, lowercase hex. Used to key the
/// stored --tune result to the exact binary that measured it.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[4 * i..4 * i + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// SHA-256 of the running binary (`/proc/self/exe`), cached.
pub fn self_sha256() -> String {
    static SHA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHA.get_or_init(|| fs::read("/proc/self/exe").map(|b| sha256_hex(&b)).unwrap_or_else(|_| "unknown".into())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMAPS: &str = "\
7f0000000000-7f0000001000 ---p 00000000 00:00 0
Size:                  4 kB
AnonHugePages:         0 kB
7f0000200000-7f0000400000 rw-p 00000000 00:00 0
Size:               2048 kB
Rss:                2048 kB
AnonHugePages:      2048 kB
THPeligible:           1
7f0000400000-7f00005ff000 rw-p 00000000 00:00 0
Size:               2044 kB
AnonHugePages:         0 kB
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
Size:                132 kB
";

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let m = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(sha256_hex(m), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    }

    #[test]
    fn smaps_parser_fixture() {
        // Inside the huge VMA, at its start and in the middle.
        assert_eq!(smaps_anon_huge_kb(SMAPS, 0x7f00_0020_0000), Some(2048));
        assert_eq!(smaps_anon_huge_kb(SMAPS, 0x7f00_0030_0000), Some(2048));
        // The VMA right after it is someone else's: 0, not ours.
        assert_eq!(smaps_anon_huge_kb(SMAPS, 0x7f00_0040_0000), Some(0));
        // A VMA without an AnonHugePages line reads 0.
        assert_eq!(smaps_anon_huge_kb(SMAPS, 0x7ffd_0000_0000), Some(0));
        // An address in no VMA.
        assert_eq!(smaps_anon_huge_kb(SMAPS, 0x1000), None);
    }

    #[test]
    fn cpu_ticks_reads_only_the_named_cpus() {
        let stat = "cpu  100 0 100 800 0 0 0 0 0 0\n\
                    cpu0 10 1 5 80 4 1 1 0 0 0\n\
                    cpu1 50 0 0 50 0 0 0 0 0 0\n\
                    intr 12345\n";
        // cpu0: busy 10+1+5+1+1+0 = 18, total 18+80+4 = 102.
        assert_eq!(cpu_ticks_in(stat, &[0]), Some((18, 102)));
        assert_eq!(cpu_ticks_in(stat, &[0, 1]), Some((68, 202)));
        assert_eq!(cpu_ticks_in(stat, &[7]), None, "a CPU absent from /proc/stat reads nothing");
        assert!(cpu_ticks(&[0]).is_some() && self_ticks().is_some());
    }

    #[test]
    fn region_is_its_own_vma_and_huge_when_thp_allows() {
        let r = Region::new(1, true).expect("map");
        assert_eq!(r.expected_huge_kb(), 2048);
        let kb = r.huge_kb().expect("smaps readable");
        // THP may be off where the tests run; the VMA must still be found and
        // hold no more than the advised span.
        assert!(kb <= r.expected_huge_kb(), "{kb} KiB counted: the VMA merged with a neighbour");
        let r4 = Region::new(2, false).expect("map");
        assert_eq!(r4.huge_kb(), Some(0));
    }
}
