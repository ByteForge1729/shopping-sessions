# shopping-sessions

An exact, multi-threaded Rust pipeline that turns messy click-stream CSV logs into two daily Parquet tables, built for the Databricks "Shopping Sessions" track of the Builders Buildathon 2026.

**Status:** working. It passes all 36 edge-case fixtures and matches the 1M-event dev answer exactly; speed work is ongoing.

## Overview

The input is CSV logs of page views, carts, purchases and ad clicks, with duplicates, malformed lines, bad timestamps and late events, plus a `links/` table that joins a shopper's many user IDs. The pipeline follows [`docs/rules.md`](docs/rules.md) exactly and writes `/output/user_daily/` (one row per person and day) and `/output/campaign_daily/` (one row per campaign and day). The score is container run time, and an answer that is almost right scores nothing.

## How it works

1. **Read:** every CSV is memory-mapped and cut into line-aligned chunks that worker threads parse in parallel (`src/csv.rs`, `src/ts.rs`). The parser implements the rules' dialect itself: quotes only around whole fields, `\r` as an ordinary character, ASCII digits only.
2. **Linked IDs** (`src/links.rs`): union-find over every valid link line; each person is named by the byte-wise smallest ID.
3. **Events** (`src/events.rs`): each copy is parsed and validated up front, then copies are partitioned by an `event_id` hash. Each partition sorts by (`event_id`, `ingest_ts`) and keeps the earliest copy, valid or not, so dedup comes before validation as the rules require.
4. **Clicks** (`src/clicks.rs`): parsed and validated. Duplicates are identical, so they cannot change a credit or an assist, and no dedup is needed.
5. **Sessions and attribution** (`src/sessions.rs`): events and clicks are partitioned by person. Each person's events are sorted by time and split on gaps over 30 minutes. A sliding window of that person's clicks over [start - 7 days, start] gives the credited click (latest, then smallest `click_id`) and the assisting campaigns.
6. **Output** (`src/output.rs`): one Parquet file per worker for `user_daily`, one for `campaign_daily`.

## Usage

```sh
cargo build --release
target/release/sessions DATA_DIR OUTPUT_DIR      # or set DATA_DIR / OUTPUT_DIR
```

Testing (needs Python with `polars` for the scripts):

```sh
python scripts/run_fixtures.py fixtures -- target/release/sessions        # the 36 edge cases
python scripts/compare.py OUTPUT_DIR DEV_ANSWER_DIR                          # dev set vs its answer
```

In Docker, with the scorer's flags: `docker build -t shopping-sessions .`, then `scripts/docker_run.sh DATA_DIR OUT_DIR`.

## Results

| Check | Result |
| --- | --- |
| Fixtures (36 edge cases) | 36/36 |
| 1M-event dev set vs published answer | exact match (117,038 and 27,400 rows) |
| Dev set run time, Apple M-series, native | 0.12 s in the pipeline |

## Limitations

- IDs are compared through 64-bit hashes (event IDs also byte for byte within a hash group). Two distinct user or click IDs with equal hashes would be merged; the chance is about 1e-6 at the final's scale.
- Not yet tuned for the scorer's 8-vCPU Xeon: parsing is scalar and the in-memory records are larger than they need to be.

## License

MIT, see [LICENSE](LICENSE). The fixtures, `docs/rules.md` and `scripts/` come from the track's starter kit.
