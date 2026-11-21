//! Events: parse, dedup on `event_id` (earliest `ingest_ts` wins, before validation),
//! validate and drop late events, then hand the survivors on grouped by person.

use std::path::Path;

use crate::csv;
use crate::data::{self, Bytes, Table, Window, PARTS};
use crate::links::People;
use crate::ts;

pub const VIEW: u8 = 0;
pub const CART: u8 = 1;
pub const PURCHASE: u8 = 2;
const LATE_MS: i64 = 86_400_000;
const MAX_PRICE: i64 = 10_000_000;

/// A surviving event, keyed by person.
#[derive(Clone, Copy)]
pub struct Ev {
    pub person: u64,
    pub ts: i64,
    pub price: u32,
    pub cat: u16,
    pub kind: u8,
    pub name: Bytes,
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
    eid_bytes: Bytes,
    user_bytes: Bytes,
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
fn parse_line(line: Bytes, lk: &Lookups) -> Option<Copy> {
    let [eid, user, ts_b, ingest_b, kind_b, product, price_b] = csv::fields::<7>(line)?;
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
        eid_bytes: eid,
        user_bytes: user,
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
    copy.cat = lk.products.index.get(product).map_or(lk.unknown_cat, |&k| lk.products.of_key[k as usize]);
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
/// into `PARTS` person partitions.
pub fn load(dir: &Path, lk: &Lookups, people: &People, threads: usize) -> Vec<Vec<Vec<Ev>>> {
    let chunks = data::load_dir(dir, 4 << 20);
    let parsed = data::par_for(
        chunks.len(),
        threads,
        || vec![Vec::<Copy>::new(); PARTS],
        |out, i| {
            for line in csv::lines(chunks[i]) {
                if let Some(copy) = parse_line(line, lk) {
                    out[data::part(copy.eid)].push(copy);
                }
            }
        },
    );
    data::par_for(
        PARTS,
        threads,
        || vec![Vec::<Ev>::new(); PARTS],
        |out, p| {
            let mut copies: Vec<Copy> = parsed.iter().flat_map(|t| t[p].iter().copied()).collect();
            copies.sort_unstable_by_key(|c| (c.eid, c.ingest));
            let mut i = 0;
            while i < copies.len() {
                let mut j = i + 1;
                while j < copies.len() && copies[j].eid == copies[i].eid {
                    j += 1;
                }
                keep(&mut copies[i..j], people, out);
                i = j;
            }
        },
    )
}

/// One hash run: normally all copies of one event (earliest ingest first). Distinct
/// IDs that share a hash are separated by their bytes.
fn keep(run: &mut [Copy], people: &People, out: &mut [Vec<Ev>]) {
    let first = run[0].eid_bytes;
    if run.iter().all(|c| c.eid_bytes == first) {
        emit(&run[0], people, out);
        return;
    }
    run.sort_by(|a, b| (a.eid_bytes, a.ingest).cmp(&(b.eid_bytes, b.ingest)));
    let mut i = 0;
    while i < run.len() {
        emit(&run[i], people, out);
        let id = run[i].eid_bytes;
        while i < run.len() && run[i].eid_bytes == id {
            i += 1;
        }
    }
}

#[inline]
fn emit(c: &Copy, people: &People, out: &mut [Vec<Ev>]) {
    if !c.ok {
        return;
    }
    let (person, name) = people.person(c.user, c.user_bytes);
    out[data::part(person)].push(Ev { person, ts: c.ts, price: c.price, cat: c.cat, kind: c.kind, name });
}
