//! Inputs that every stage shares: memory-mapped CSV files, the window, the product
//! and campaign tables, a fast ID hash and a small work-stealing thread pool.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::csv;
use crate::ts;

pub type Bytes = &'static [u8];
pub const PARTS: usize = 256;

/// A `Bytes` in 8 bytes: a 48-bit address and a 16-bit length. A slice of 65,535
/// bytes or more is boxed once and referenced indirectly (length field 0xFFFF).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ref(u64);

const INDIRECT: u64 = 0xFFFF;

impl Ref {
    #[inline]
    pub fn new(b: Bytes) -> Ref {
        if (b.len() as u64) < INDIRECT {
            Ref((b.as_ptr() as u64 & 0xFFFF_FFFF_FFFF) | (b.len() as u64) << 48)
        } else {
            let boxed: &'static Bytes = Box::leak(Box::new(b));
            Ref((boxed as *const Bytes as u64 & 0xFFFF_FFFF_FFFF) | INDIRECT << 48)
        }
    }

    #[inline]
    pub fn get(self) -> Bytes {
        let (addr, len) = ((self.0 & 0xFFFF_FFFF_FFFF) as usize, self.0 >> 48);
        // SAFETY: built by `new` from a 'static slice (mmaps and boxes are leaked).
        unsafe {
            if len == INDIRECT {
                *(addr as *const Bytes)
            } else {
                std::slice::from_raw_parts(addr as *const u8, len as usize)
            }
        }
    }
}

/// Keys are already `h64` hashes: pass them through.
#[derive(Default, Clone, Copy)]
pub struct IdHash(u64);

impl std::hash::Hasher for IdHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!("IdHash only hashes u64 keys")
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

pub type FastMap<V> = HashMap<u64, V, std::hash::BuildHasherDefault<IdHash>>;

/// 64-bit hash of an ID (whole 8-byte words, then a strong finaliser). IDs that hash
/// alike are treated as one ID, so this needs to be good: 2^-64 per pair.
#[inline]
pub fn h64(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h: u64 = (b.len() as u64).wrapping_mul(K) ^ 0x2545_F491_4F6C_DD1D;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().unwrap());
        h = (h ^ w).wrapping_mul(K).rotate_left(29);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut buf = [0u8; 8];
        buf[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(buf)).wrapping_mul(K).rotate_left(29);
    }
    fmix(h)
}

#[inline]
pub fn fmix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

#[inline]
pub fn part(h: u64) -> usize {
    (h >> 56) as usize
}

/// Memory-map one file for the whole run (the mapping is leaked on purpose).
pub fn map(path: &Path) -> Bytes {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    if file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        return &[];
    }
    let mmap = unsafe { memmap2::Mmap::map(&file) }.unwrap_or_else(|e| panic!("mmap {}: {e}", path.display()));
    let leaked: &'static memmap2::Mmap = Box::leak(Box::new(mmap));
    &leaked[..]
}

/// Bodies (header removed) of every `*.csv` in a folder, cut into line-aligned chunks.
pub fn load_dir(dir: &Path, chunk: usize) -> Vec<Bytes> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file());
    paths.sort();
    paths.iter().flat_map(|p| csv::chunks(csv::body(map(p)), chunk)).collect()
}

/// Run `f(i)` for every i in 0..n on `threads` threads, each thread with its own
/// state from `init`; returns the states.
pub fn par_for<S: Send, I: Fn() -> S + Sync, F: Fn(&mut S, usize) + Sync>(
    n: usize,
    threads: usize,
    init: I,
    f: F,
) -> Vec<S> {
    let next = AtomicUsize::new(0);
    let out = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut state = init();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    f(&mut state, i);
                }
                out.lock().unwrap().push(state);
            });
        }
    });
    out.into_inner().unwrap()
}

pub struct Window {
    pub start: i64,
    pub end: i64,
}

pub fn window(data: &Path) -> Window {
    let text = std::fs::read_to_string(data.join("manifest.json")).expect("manifest.json");
    let get = |key: &str| -> i64 {
        let at = text.find(&format!("\"{key}\"")).unwrap_or_else(|| panic!("{key} missing"));
        let rest = &text[at + key.len() + 2..];
        let open = rest.find('"').expect("value") + 1;
        let close = rest[open..].find('"').expect("value end") + open;
        ts::parse(rest[open..close].as_bytes()).unwrap_or_else(|| panic!("bad {key}"))
    };
    Window { start: get("window_start"), end: get("window_end") }
}

/// A two-column lookup table (`products.csv`, `campaigns.csv`): key -> value index,
/// and the distinct values in first-seen order.
pub struct Table {
    pub index: HashMap<Bytes, u16>,
    pub values: Vec<Bytes>,
    pub of_key: Vec<u16>,
    pub keys: Vec<Bytes>,
    /// Open addressing on the key hash: (hash, key index), index u32::MAX when empty.
    slots: Vec<(u64, u32)>,
    exact: bool,
}

impl Table {
    /// Key index of `key`. Keys are matched by their 64-bit hash, like every other
    /// ID here; if two keys of the file share a hash, lookups compare bytes instead.
    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<u16> {
        if self.exact {
            return self.index.get(key).copied();
        }
        let h = h64(key);
        let mask = self.slots.len() - 1;
        let mut s = h as usize & mask;
        loop {
            let (k, i) = self.slots[s];
            if i == u32::MAX {
                return None;
            }
            if k == h {
                return Some(i as u16);
            }
            s = (s + 1) & mask;
        }
    }
}

pub fn table(path: &Path) -> Table {
    let t = table_slow(path);
    let cap = (t.keys.len() * 2).next_power_of_two().max(16);
    let mut slots = vec![(0u64, u32::MAX); cap];
    let mut exact = false;
    for (i, k) in t.keys.iter().enumerate() {
        let h = h64(k);
        let mut s = h as usize & (cap - 1);
        while slots[s].1 != u32::MAX {
            exact |= slots[s].0 == h;
            s = (s + 1) & (cap - 1);
        }
        slots[s] = (h, i as u32);
    }
    Table { slots, exact, ..t }
}

fn table_slow(path: &Path) -> Table {
    let body = csv::body(map(path));
    let mut index = HashMap::new();
    let mut values: Vec<Bytes> = Vec::new();
    let mut value_ids: HashMap<Bytes, u16> = HashMap::new();
    let mut of_key = Vec::new();
    let mut keys = Vec::new();
    for line in csv::lines(body) {
        let Some([key, value]) = csv::fields::<2>(line) else { continue };
        if key.is_empty() || index.contains_key(key) {
            continue;
        }
        let next = values.len() as u16;
        let vid = *value_ids.entry(value).or_insert_with(|| {
            values.push(value);
            next
        });
        index.insert(key, of_key.len() as u16);
        of_key.push(vid);
        keys.push(key);
    }
    Table { index, values, of_key, keys, slots: Vec::new(), exact: true }
}

pub fn threads() -> usize {
    std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(8)
        .clamp(1, 64)
}
