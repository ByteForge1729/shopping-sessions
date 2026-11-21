//! Sessions, attribution and the daily aggregates, one person at a time.
//!
//! A person's events sorted by `ts` split where the gap exceeds 30 minutes. Their
//! valid clicks, sorted by (`ts`, `click_id`), form a sliding window
//! [start - 7 days, start] as sessions advance: its last click (smallest ID among
//! those at the latest `ts`) credits the session, and every campaign in it assists.

use crate::clicks::{Click, LOOKBACK_MS};
use crate::data::{self, Bytes, PARTS};
use crate::events::{Ev, PURCHASE};
use crate::ts::MS_PER_DAY;

const GAP_MS: i64 = 30 * 60 * 1000;
pub const NO_CATEGORY: u16 = u16::MAX;

pub struct UserRow {
    pub name: Bytes,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: i64,
    pub top: u16,
}

/// Sizes shared by every worker. Campaign `campaigns` (one past the last) is organic.
pub struct Dims {
    pub campaigns: usize,
    pub first_day: i64,
    pub days: usize,
    pub categories: usize,
    /// rank[c]: position of category c's name in byte order (ties go to the smaller).
    pub rank: Vec<u32>,
}

/// Per campaign and day: credited sessions, purchasing sessions, revenue, assisted.
pub type CampaignDay = [i64; 4];

pub struct Worker {
    pub rows: Vec<UserRow>,
    pub campaign_days: Vec<CampaignDay>,
    in_window: Vec<u32>,
    active: Vec<u16>,
    slot: Vec<u32>,
    category_sum: Vec<i64>,
    touched: Vec<u16>,
}

impl Worker {
    fn new(d: &Dims) -> Worker {
        Worker {
            rows: Vec::new(),
            campaign_days: vec![[0; 4]; (d.campaigns + 1) * d.days],
            in_window: vec![0; d.campaigns],
            active: Vec::new(),
            slot: vec![0; d.campaigns],
            category_sum: vec![0; d.categories],
            touched: Vec::new(),
        }
    }

    #[inline]
    fn enter(&mut self, c: u16) {
        let n = &mut self.in_window[c as usize];
        *n += 1;
        if *n == 1 {
            self.slot[c as usize] = self.active.len() as u32;
            self.active.push(c);
        }
    }

    #[inline]
    fn leave(&mut self, c: u16) {
        let n = &mut self.in_window[c as usize];
        *n -= 1;
        if *n == 0 {
            let at = self.slot[c as usize] as usize;
            let last = self.active.pop().unwrap();
            if at < self.active.len() {
                self.active[at] = last;
                self.slot[last as usize] = at as u32;
            }
        }
    }

    fn clear_window(&mut self) {
        for &c in &self.active {
            self.in_window[c as usize] = 0;
        }
        self.active.clear();
    }

    /// The day's row from the categories touched so far, then reset them.
    fn flush(&mut self, d: &Dims, row: &mut UserRow) {
        let mut top = NO_CATEGORY;
        let mut best = -1i64;
        for &c in &self.touched {
            let sum = self.category_sum[c as usize];
            if sum > best || (sum == best && d.rank[c as usize] < d.rank[top as usize]) {
                best = sum;
                top = c;
            }
            self.category_sum[c as usize] = 0;
        }
        self.touched.clear();
        row.top = top;
        self.rows.push(std::mem::replace(row, UserRow { name: row.name, day: 0, sessions: 0, events: 0, revenue: 0, top: 0 }));
    }

    fn person(&mut self, d: &Dims, evs: &[Ev], clicks: &[Click]) {
        let organic = d.campaigns;
        let mut row = UserRow { name: evs[0].name.get(), day: i32::MIN, sessions: 0, events: 0, revenue: 0, top: 0 };
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut i = 0;
        while i < evs.len() {
            let start = evs[i].ts;
            let day = start.div_euclid(MS_PER_DAY);
            if row.day != day as i32 {
                if row.sessions > 0 {
                    self.flush(d, &mut row);
                }
                row.day = day as i32;
            }
            let (mut revenue, mut purchased, mut prev, mut j) = (0i64, false, start, i);
            while j < evs.len() && evs[j].ts - prev <= GAP_MS {
                let e = &evs[j];
                if e.kind == PURCHASE {
                    revenue += e.price as i64;
                    purchased = true;
                    let sum = &mut self.category_sum[e.cat as usize];
                    if *sum == 0 {
                        self.touched.push(e.cat);
                    }
                    *sum += e.price as i64;
                }
                prev = e.ts;
                j += 1;
            }
            row.sessions += 1;
            row.events += (j - i) as u32;
            row.revenue += revenue;

            while hi < clicks.len() && clicks[hi].ts <= start {
                self.enter(clicks[hi].campaign);
                hi += 1;
            }
            while lo < hi && clicks[lo].ts < start - LOOKBACK_MS {
                self.leave(clicks[lo].campaign);
                lo += 1;
            }
            let credited = if lo < hi {
                let latest = clicks[hi - 1].ts;
                let mut k = hi - 1;
                while k > lo && clicks[k - 1].ts == latest {
                    k -= 1;
                }
                clicks[k].campaign as usize
            } else {
                organic
            };
            let di = (day - d.first_day) as usize;
            let cd = &mut self.campaign_days[credited * d.days + di];
            cd[0] += 1;
            cd[1] += purchased as i64;
            cd[2] += revenue;
            for k in 0..self.active.len() {
                let a = self.active[k] as usize;
                self.campaign_days[a * d.days + di][3] += 1;
            }
            i = j;
        }
        if row.sessions > 0 {
            self.flush(d, &mut row);
        }
        self.clear_window();
    }
}

/// Process every person partition; each worker returns its user rows and its share
/// of the campaign-day counters.
pub fn run(events: &[Vec<Vec<Ev>>], clicks: &[Vec<Vec<Click>>], d: &Dims, threads: usize) -> Vec<Worker> {
    data::par_for(
        PARTS,
        threads,
        || Worker::new(d),
        |w, q| {
            let mut evs: Vec<Ev> = events.iter().flat_map(|t| t[q].iter().copied()).collect();
            evs.sort_unstable_by_key(|e| (e.person, e.ts));
            let mut cl: Vec<Click> = clicks.iter().flat_map(|t| t[q].iter().copied()).collect();
            cl.sort_unstable_by(|a, b| (a.person, a.ts).cmp(&(b.person, b.ts)).then_with(|| a.id.get().cmp(b.id.get())));
            let (mut i, mut c) = (0, 0);
            while i < evs.len() {
                let person = evs[i].person;
                let mut j = i + 1;
                while j < evs.len() && evs[j].person == person {
                    j += 1;
                }
                while c < cl.len() && cl[c].person < person {
                    c += 1;
                }
                let mut ce = c;
                while ce < cl.len() && cl[ce].person == person {
                    ce += 1;
                }
                w.person(d, &evs[i..j], &cl[c..ce]);
                c = ce;
                i = j;
            }
        },
    )
}
