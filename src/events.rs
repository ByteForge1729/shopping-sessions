//! Events: parse, dedup on `event_id` (earliest `ingest_ts` wins, before validation),
//! validate and drop late events, then hand the survivors on grouped by person.

use std::path::Path;

use crate::csv;
use crate::data::{self, Bytes, Ref, Table, Window, PARTS};
use crate::links::People;
use crate::ts;

pub const VIEW: u8 = 0;
pub const CART: u8 = 1;
pub const PURCHASE: u8 = 2;
const LATE_MS: i64 = 86_400_000;
const MAX_PRICE: i64 = 10_000_000;

/// A surviving event, keyed by person (by user ID hash before [`resolve`]).
#[derive(Clone, Copy)]
pub struct Ev {
    pub person: u64,
    pub ts: i64,
    pub price: u32,
    pub cat: u16,
    pub kind: u8,
    pub name: Ref,
}

/// One parsed copy of an event, valid or not (validity is decided on the kept copy).
#[derive(Clone, Copy)]
struct Copy {
    eid: u64,
    ingest: i64,
    user: u64,
    ts: i64,
    price: u32,
    cat: u16,
    kind: u8,
    ok: bool,
    eid_bytes: Ref,
    user_bytes: Ref,
}

pub struct Lookups<'a> {
    pub window: &'a Window,
    pub products: &'a Table,
    pub unknown_cat: u16,
}

#[inline]
fn kind_of(b: &[u8]) -> Option<u8> {
    match b {
        b"view" => Some(VIEW),
        b"cart" => Some(CART),
        b"purchase" => Some(PURCHASE),
        _ => None,
    }
}

#[inline]
fn parse_fields(f: [Bytes; 7], lk: &Lookups) -> Option<Copy> {
    let [eid, user, ts_b, ingest_b, kind_b, product, price_b] = f;
    if eid.is_empty() {
        return None;
    }
    let ingest = ts::parse(ingest_b).unwrap_or(i64::MAX);
    let mut copy = Copy {
        eid: data::h64(eid),
        ingest,
        user: 0,
        ts: 0,
        price: 0,
        cat: 0,
        kind: 0,
        ok: false,
        eid_bytes: Ref::new(eid),
        user_bytes: Ref::new(user),
    };
    let Some(t) = ts::parse(ts_b) else { return Some(copy) };
    let Some(kind) = kind_of(kind_b) else { return Some(copy) };
    if user.is_empty() || product.is_empty() || t < lk.window.start || t >= lk.window.end {
        return Some(copy);
    }
    if ingest.saturating_sub(t) > LATE_MS {
        return Some(copy);
    }
    if kind == PURCHASE {
        match price(price_b) {
            Some(p) => copy.price = p,
            None => return Some(copy),
        }
    }
    copy.cat = lk.products.get(product).map_or(lk.unknown_cat, |k| lk.products.of_key[k as usize]);
    copy.ts = t;
    copy.kind = kind;
    copy.user = data::h64(user);
    copy.ok = true;
    Some(copy)
}

/// A purchase price: ASCII digits only (leading zeros fine), value 1 to 10,000,000.
#[inline]
fn price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let significant = &b[b.iter().position(|&c| c != b'0').unwrap_or(b.len())..];
    if significant.len() > 8 {
        return None;
    }
    let value = ts::digits(significant).unwrap_or(0);
    (1..=MAX_PRICE).contains(&value).then_some(value as u32)
}

/// Parse, dedup and validate every event; returns, per worker, the survivors split
/// into `PARTS` user partitions (see [`resolve`]).
pub fn load(dir: &Path, lk: &Lookups, threads: usize) -> Vec<Vec<Vec<Ev>>> {
    let clock = std::time::Instant::now();
    let chunks = data::load_dir(dir, 4 << 20);
    let parsed = data::par_for(
        chunks.len(),
        threads,
        || vec![Vec::<Copy>::new(); PARTS],
        |out, i| {
            csv::records::<7>(chunks[i], |f| {
                if let Some(copy) = parse_fields(f, lk) {
                    out[data::part(copy.eid)].push(copy);
                }
            });
        },
    );
    eprintln!("   events parsed in {:.3}s", clock.elapsed().as_secs_f64());
    data::par_for(
        PARTS,
        threads,
        || vec![Vec::<Ev>::new(); PARTS],
        |out, p| {
            let lists: Vec<&[Copy]> = parsed.iter().map(|t| &t[p][..]).collect();
            for at in earliest_copies(&lists) {
                emit(&lists[(at >> 24) as usize][(at & 0xFF_FFFF) as usize], out);
            }
        },
    )
}

const EMPTY: u32 = u32::MAX;

/// (list << 24 | index) of the earliest-ingested copy of every distinct event ID,
/// from an open-addressing table keyed by the ID hash (its low bits; the partition
/// used the high ones). IDs that share a hash are told apart by their bytes. Copies
/// with equal `ingest_ts` are identical, so either may be kept.
fn earliest_copies(lists: &[&[Copy]]) -> impl Iterator<Item = u32> {
    let total: usize = lists.iter().map(|l| l.len()).sum();
    let cap = (total * 2).next_power_of_two().max(16);
    let mask = cap - 1;
    let mut slots: Vec<(u64, u32)> = vec![(0, EMPTY); cap];
    let copy = |at: u32| &lists[(at >> 24) as usize][(at & 0xFF_FFFF) as usize];
    for (t, list) in lists.iter().enumerate() {
        assert!(list.len() < 1 << 24 && t < 255, "partition too large for slot packing");
        for (i, c) in list.iter().enumerate() {
            let me = (t as u32) << 24 | i as u32;
            let mut s = c.eid as usize & mask;
            loop {
                let slot = &mut slots[s];
                if slot.1 == EMPTY {
                    *slot = (c.eid, me);
                    break;
                }
                if slot.0 == c.eid {
                    let kept = copy(slot.1);
                    if kept.eid_bytes == c.eid_bytes || kept.eid_bytes.get() == c.eid_bytes.get() {
                        if c.ingest < kept.ingest {
                            slot.1 = me;
                        }
                        break;
                    }
                }
                s = (s + 1) & mask;
            }
        }
    }
    slots.into_iter().filter(|s| s.1 != EMPTY).map(|s| s.1)
}

#[inline]
fn emit(c: &Copy, out: &mut [Vec<Ev>]) {
    if c.ok {
        out[data::part(c.user)].push(Ev { person: c.user, ts: c.ts, price: c.price, cat: c.cat, kind: c.kind, name: c.user_bytes });
    }
}

/// Re-key events from user to person, one user partition at a time (so only that
/// partition's slice of the link map is touched), regrouped by person partition.
pub fn resolve(by_user: &[Vec<Vec<Ev>>], people: &People, threads: usize) -> Vec<Vec<Vec<Ev>>> {
    data::par_for(
        PARTS,
        threads,
        || vec![Vec::<Ev>::new(); PARTS],
        |out, u| {
            for worker in by_user {
                for e in &worker[u] {
                    let (person, name) = people.person(e.person, e.name.get());
                    out[data::part(person)].push(Ev { person, name: Ref::new(name), ..*e });
                }
            }
        },
    )
}
