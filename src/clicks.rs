//! Clicks: parse and validate, then group by person. Copies of a click are identical,
//! so they choose the same credit and add no assist: no dedup is needed.

use std::path::Path;

use crate::csv;
use crate::data::{self, Bytes, Table, Window, PARTS};
use crate::links::People;
use crate::ts;

pub const LOOKBACK_MS: i64 = 7 * ts::MS_PER_DAY;

#[derive(Clone, Copy)]
pub struct Click {
    pub person: u64,
    pub ts: i64,
    pub campaign: u16,
    pub id: Bytes,
}

#[inline]
fn parse_line(line: Bytes, window: &Window, campaigns: &Table) -> Option<(u64, Bytes, Click)> {
    let [id, user, campaign, ts_b] = csv::fields::<4>(line)?;
    if id.is_empty() || user.is_empty() {
        return None;
    }
    let t = ts::parse(ts_b)?;
    if t < window.start - LOOKBACK_MS || t >= window.end {
        return None;
    }
    let &campaign = campaigns.index.get(campaign)?;
    Some((data::h64(user), user, Click { person: 0, ts: t, campaign, id }))
}

/// Every valid click, per worker, split into `PARTS` person partitions.
pub fn load(dir: &Path, window: &Window, campaigns: &Table, people: &People, threads: usize) -> Vec<Vec<Vec<Click>>> {
    let chunks = data::load_dir(dir, 4 << 20);
    data::par_for(
        chunks.len(),
        threads,
        || vec![Vec::<Click>::new(); PARTS],
        |out, i| {
            for line in csv::lines(chunks[i]) {
                if let Some((user, user_bytes, mut click)) = parse_line(line, window, campaigns) {
                    click.person = people.person(user, user_bytes).0;
                    out[data::part(click.person)].push(click);
                }
            }
        },
    )
}
