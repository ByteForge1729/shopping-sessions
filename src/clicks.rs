//! Clicks: parse and validate, then group by person. Copies of a click are identical,
//! so they choose the same credit and add no assist: no dedup is needed.

use crate::csv;
use crate::data::{self, Bytes, Ref, Table, Window, PARTS};
use crate::links::People;
use crate::ts;

pub const LOOKBACK_MS: i64 = 7 * ts::MS_PER_DAY;

#[derive(Clone, Copy)]
pub struct Click {
    pub person: u64,
    pub ts: i64,
    pub campaign: u16,
    pub id: Ref,
}

#[inline]
fn parse_fields(f: [Bytes; 4], window: &Window, campaigns: &Table) -> Option<(u64, Bytes, Click)> {
    let [id, user, campaign, ts_b] = f;
    if id.is_empty() || user.is_empty() {
        return None;
    }
    let t = ts::parse(ts_b)?;
    if t < window.start - LOOKBACK_MS || t >= window.end {
        return None;
    }
    let campaign = campaigns.get(campaign)?;
    Some((data::h64(user), user, Click { person: 0, ts: t, campaign, id: Ref::new(id) }))
}

/// One worker's valid clicks, split into `PARTS` user partitions (see [`resolve`]).
pub struct Parsed(pub Vec<Vec<Click>>);

impl Parsed {
    pub fn new() -> Self {
        Parsed(vec![Vec::new(); PARTS])
    }

    pub fn chunk(&mut self, chunk: Bytes, window: &Window, campaigns: &Table) {
        crate::events::reserve(&mut self.0, chunk.len() / 45);
        csv::records::<4>(chunk, |f| {
            if let Some((user, _, mut click)) = parse_fields(f, window, campaigns) {
                click.person = user;
                self.0[data::part(user)].push(click);
            }
        });
    }
}

/// Re-key clicks from user to person, one user partition at a time.
pub fn resolve(by_user: &[&Parsed], people: &People, threads: usize) -> Vec<Vec<Vec<Click>>> {
    data::par_for(
        PARTS,
        threads,
        || vec![Vec::<Click>::new(); PARTS],
        |out, u| {
            crate::events::reserve(out, by_user.iter().map(|w| w.0[u].len()).sum());
            for worker in by_user {
                for c in &worker.0[u] {
                    let person = people.person(c.person, &[]).0;
                    out[data::part(person)].push(Click { person, ..*c });
                }
            }
        },
    )
}
