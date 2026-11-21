//! Clicks: parse and validate, then group by person. Copies of a click are identical,
//! so they choose the same credit and add no assist: no dedup is needed.

use std::path::Path;

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

/// Every valid click, per worker, split into `PARTS` user partitions (see [`resolve`]).
pub fn load(dir: &Path, window: &Window, campaigns: &Table, threads: usize) -> Vec<Vec<Vec<Click>>> {
    let chunks = data::load_dir(dir, 4 << 20);
    data::par_for(
        chunks.len(),
        threads,
        || vec![Vec::<Click>::new(); PARTS],
        |out, i| {
            csv::records::<4>(chunks[i], |f| {
                if let Some((user, _, mut click)) = parse_fields(f, window, campaigns) {
                    click.person = user;
                    out[data::part(user)].push(click);
                }
            });
        },
    )
}

/// Re-key clicks from user to person, one user partition at a time.
pub fn resolve(by_user: &[Vec<Vec<Click>>], people: &People, threads: usize) -> Vec<Vec<Vec<Click>>> {
    data::par_for(
        PARTS,
        threads,
        || vec![Vec::<Click>::new(); PARTS],
        |out, u| {
            for worker in by_user {
                for c in &worker[u] {
                    let person = people.person(c.person, &[]).0;
                    out[data::part(person)].push(Click { person, ..*c });
                }
            }
        },
    )
}
