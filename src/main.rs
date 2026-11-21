//! Shopping Sessions: clean the click-stream logs, join linked IDs, build sessions,
//! attribute them to campaigns and write the two daily Parquet tables.
//!
//! Usage: `sessions [DATA_DIR OUTPUT_DIR]`, else `$DATA_DIR` and `$OUTPUT_DIR`.

mod clicks;
mod csv;
mod data;
mod events;
mod links;
mod output;
mod scan;
mod sessions;
mod ts;

use std::path::PathBuf;
use std::time::Instant;

use data::Bytes;

const CHUNK: usize = 4 << 20;

#[derive(Clone, Copy)]
enum Input {
    Links,
    Events,
    Clicks,
}

/// One worker's share of every parsed input.
struct Parsed {
    links: links::Parsed,
    events: events::Parsed,
    clicks: clicks::Parsed,
}

impl Parsed {
    fn new() -> Self {
        Parsed { links: links::Parsed::new(), events: events::Parsed::new(), clicks: clicks::Parsed::new() }
    }
}

fn dirs() -> (PathBuf, PathBuf) {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 {
        return (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]));
    }
    let env = |k: &str, default: &str| PathBuf::from(std::env::var(k).unwrap_or_else(|_| default.to_string()));
    (env("DATA_DIR", "/data"), env("OUTPUT_DIR", "/output"))
}

fn main() {
    let clock = Instant::now();
    let lap = |what: &str| eprintln!("{:>8.3}s {what}", clock.elapsed().as_secs_f64());
    let (data_dir, out_dir) = dirs();
    let threads = data::threads();

    let window = data::window(&data_dir);
    let products = data::table(&data_dir.join("products.csv"));
    let campaigns = data::table(&data_dir.join("campaigns.csv"));
    let mut category_names: Vec<Bytes> = products.values.clone();
    let unknown_cat = category_names.len() as u16;
    category_names.push(b"unknown");
    let lookups = events::Lookups { window: &window, products: &products, unknown_cat };

    // Every chunk of every input in one work list, largest first, so no worker
    // waits at a stage boundary while another finishes a big file.
    let mut work: Vec<(Input, Bytes)> = Vec::new();
    for (input, dir) in [(Input::Links, "links"), (Input::Events, "events"), (Input::Clicks, "clicks")] {
        work.extend(data::load_dir(&data_dir.join(dir), CHUNK).into_iter().map(|c| (input, c)));
    }
    work.sort_by_key(|w| std::cmp::Reverse(w.1.len()));
    let parsed = data::par_for(work.len(), threads, Parsed::new, |p, i| match work[i] {
        (Input::Links, c) => p.links.chunk(c),
        (Input::Events, c) => p.events.chunk(c, &lookups),
        (Input::Clicks, c) => p.clicks.chunk(c, &window, &campaigns),
    });
    lap("parsed");
    let people = links::People::build(&parsed.iter().map(|p| &p.links).collect::<Vec<_>>(), threads);
    lap("links");
    let evs = events::dedup(&parsed.iter().map(|p| &p.events).collect::<Vec<_>>(), threads);
    lap("dedup");
    let evs = events::resolve(&evs, &people, threads);
    let cls = clicks::resolve(&parsed.iter().map(|p| &p.clicks).collect::<Vec<_>>(), &people, threads);
    drop(parsed);
    lap("people");

    let mut order: Vec<usize> = (0..category_names.len()).collect();
    order.sort_by_key(|&c| category_names[c]);
    let mut rank = vec![0u32; category_names.len()];
    for (r, &c) in order.iter().enumerate() {
        rank[c] = r as u32;
    }
    let first_day = window.start.div_euclid(ts::MS_PER_DAY);
    let last_day = (window.end - 1).div_euclid(ts::MS_PER_DAY);
    let dims = sessions::Dims {
        campaigns: campaigns.keys.len(),
        first_day,
        days: (last_day - first_day + 1).max(1) as usize,
        categories: category_names.len(),
        rank,
    };
    let workers = sessions::run(&evs, &cls, &dims, threads);
    drop(evs);
    drop(cls);
    lap("sessions");

    let users = out_dir.join("user_daily");
    let camps = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&users).expect("create user_daily");
    std::fs::create_dir_all(&camps).expect("create campaign_daily");
    let mut counts = vec![[0i64; 4]; (dims.campaigns + 1) * dims.days];
    for w in &workers {
        for (total, part) in counts.iter_mut().zip(&w.campaign_days) {
            for k in 0..4 {
                total[k] += part[k];
            }
        }
    }
    let channels: Vec<Bytes> = campaigns.of_key.iter().map(|&v| campaigns.values[v as usize]).collect();
    std::thread::scope(|scope| {
        for (i, w) in workers.iter().enumerate() {
            let (users, names) = (&users, &category_names);
            scope.spawn(move || {
                output::write_users(&users.join(format!("part-{i:03}.parquet")), &w.rows, names).expect("write user_daily");
            });
        }
        output::write_campaigns(&camps.join("part-000.parquet"), &counts, &dims, &campaigns.keys, &channels)
            .expect("write campaign_daily");
    });
    lap("written");
    // Everything is on disk: skip freeing gigabytes of buffers one by one.
    std::process::exit(0);
}
