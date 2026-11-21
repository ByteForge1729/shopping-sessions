//! Linked IDs: union-find over every valid `links/` line, each person named by the
//! byte-wise smallest of their IDs. IDs in no valid line are people on their own.
//!
//! Everything runs in parallel: IDs are interned per hash partition, unions use a
//! lock-free union-find (a root always links under the smaller index, so no cycles),
//! and the result is one small map per partition of user-ID hashes, so a stage that
//! works on one partition only touches a map that fits in cache.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::csv;
use crate::data::{self, Bytes, FastMap, PARTS};

type Shard = FastMap<(u64, Bytes)>;

/// user ID hash -> (person key, person name), for linked IDs only, sharded by
/// `part(user hash)`. A person key is the hash of the person's name, so an unlinked
/// ID's key is its own hash.
pub struct People {
    shards: Vec<Shard>,
}

impl People {
    #[inline]
    pub fn person(&self, user_hash: u64, user: Bytes) -> (u64, Bytes) {
        self.shards[data::part(user_hash)].get(&user_hash).copied().unwrap_or((user_hash, user))
    }

    /// Join every worker's parsed link lines into people.
    pub fn build(parsed: &[&Parsed], threads: usize) -> People {
        let ids = intern(parsed, threads);
        let total = ids.base[PARTS] as usize;
        let parent: Vec<AtomicU32> = (0..total as u32).map(AtomicU32::new).collect();
        data::par_for(parsed.len(), threads, || (), |_, t| {
            for &(a, b) in &parsed[t].pairs {
                unite(&parent, ids.gid(a), ids.gid(b));
            }
        });
        let best: Vec<AtomicU32> = (0..total as u32).map(AtomicU32::new).collect();
        let names = &ids.names;
        data::par_for(PARTS, threads, || (), |_, p| {
            for g in ids.base[p]..ids.base[p + 1] {
                let root = find(&parent, g) as usize;
                let mut cur = best[root].load(Ordering::Relaxed);
                while names[g as usize] < names[cur as usize] {
                    match best[root].compare_exchange_weak(cur, g, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => break,
                        Err(seen) => cur = seen,
                    }
                }
            }
        });
        let mut shards: Vec<Option<Shard>> = (0..PARTS).map(|_| None).collect();
        let built = data::par_for(PARTS, threads, Vec::new, |out: &mut Vec<(usize, Shard)>, p| {
            let mut shard = Shard::default();
            shard.reserve((ids.base[p + 1] - ids.base[p]) as usize);
            for g in ids.base[p]..ids.base[p + 1] {
                let root = find(&parent, g) as usize;
                let name = names[best[root].load(Ordering::Relaxed) as usize];
                shard.insert(data::h64(names[g as usize]), (data::h64(name), name));
            }
            out.push((p, shard));
        });
        for (p, shard) in built.into_iter().flatten() {
            shards[p] = Some(shard);
        }
        People { shards: shards.into_iter().map(Option::unwrap_or_default).collect() }
    }
}

/// One worker's link lines: ID hashes per partition (with their bytes) and pairs.
pub struct Parsed {
    ids: Vec<Vec<(u64, Bytes)>>,
    pairs: Vec<(u64, u64)>,
}

impl Parsed {
    pub fn new() -> Self {
        Parsed { ids: vec![Vec::new(); PARTS], pairs: Vec::new() }
    }

    pub fn chunk(&mut self, chunk: Bytes) {
        csv::records::<2>(chunk, |[a, b]| {
            if !a.is_empty() && !b.is_empty() {
                self.add(a, b);
            }
        });
    }

    fn add(&mut self, a: Bytes, b: Bytes) {
        let (ha, hb) = (data::h64(a), data::h64(b));
        self.ids[data::part(ha)].push((ha, a));
        self.ids[data::part(hb)].push((hb, b));
        self.pairs.push((ha, hb));
    }
}

/// Dense global IDs: partition p owns `base[p]..base[p + 1]`.
struct Ids {
    base: Vec<u32>,
    names: Vec<Bytes>,
    local: Vec<FastMap<u32>>,
}

impl Ids {
    #[inline]
    fn gid(&self, h: u64) -> u32 {
        let p = data::part(h);
        self.base[p] + self.local[p][&h]
    }
}

fn intern(parsed: &[&Parsed], threads: usize) -> Ids {
    let per_part = data::par_for(PARTS, threads, Vec::new, |out: &mut Vec<(usize, FastMap<u32>, Vec<Bytes>)>, p| {
        let mut map = FastMap::default();
        let mut names = Vec::new();
        for worker in parsed {
            for &(h, name) in &worker.ids[p] {
                map.entry(h).or_insert_with(|| {
                    names.push(name);
                    (names.len() - 1) as u32
                });
            }
        }
        out.push((p, map, names));
    });
    let mut slots: Vec<Option<(FastMap<u32>, Vec<Bytes>)>> = (0..PARTS).map(|_| None).collect();
    for (p, map, names) in per_part.into_iter().flatten() {
        slots[p] = Some((map, names));
    }
    let mut base = vec![0u32; PARTS + 1];
    let mut all_names = Vec::new();
    let mut local = Vec::with_capacity(PARTS);
    for (p, slot) in slots.into_iter().enumerate() {
        let (map, names) = slot.unwrap_or_default();
        base[p + 1] = base[p] + names.len() as u32;
        all_names.extend(names);
        local.push(map);
    }
    Ids { base, names: all_names, local }
}

/// Root of `x`, halving the path as it goes (safe under concurrent unions: a parent
/// pointer only ever moves to a smaller index in the same set).
#[inline]
fn find(parent: &[AtomicU32], mut x: u32) -> u32 {
    loop {
        let p = parent[x as usize].load(Ordering::Relaxed);
        if p == x {
            return x;
        }
        let gp = parent[p as usize].load(Ordering::Relaxed);
        if gp != p {
            let _ = parent[x as usize].compare_exchange_weak(p, gp, Ordering::Relaxed, Ordering::Relaxed);
        }
        x = gp;
    }
}

#[inline]
fn unite(parent: &[AtomicU32], a: u32, b: u32) {
    loop {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra == rb {
            return;
        }
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        if parent[hi as usize].compare_exchange(hi, lo, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return;
        }
    }
}
