//! Parquet output: one file per worker for `user_daily`, one for `campaign_daily`.
//! Plain encoding, no compression: the files are small and only written once.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::errors::Result;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

use crate::data::Bytes;
use crate::sessions::{CampaignDay, Dims, UserRow, NO_CATEGORY};

const USER_SCHEMA: &str = "message user_daily {
    required binary user_id (STRING);
    required int32 day (DATE);
    required int64 sessions;
    required int64 events;
    required int64 revenue_cents;
    optional binary top_category (STRING);
}";

const CAMPAIGN_SCHEMA: &str = "message campaign_daily {
    required binary campaign_id (STRING);
    required binary channel (STRING);
    required int32 day (DATE);
    required int64 sessions;
    required int64 purchasing_sessions;
    required int64 revenue_cents;
    required int64 assisted_sessions;
}";

/// One column's values, ready for a column writer.
enum Column {
    Text(Vec<ByteArray>, Option<Vec<i16>>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
}

fn bytes(b: &[u8]) -> ByteArray {
    ByteArray::from(b.to_vec())
}

fn write(path: &Path, schema: &str, columns: Vec<Column>) -> Result<()> {
    let schema = Arc::new(parse_message_type(schema)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .build(),
    );
    let mut writer = SerializedFileWriter::new(File::create(path)?, schema, props)?;
    let mut group = writer.next_row_group()?;
    let mut columns = columns.into_iter();
    while let Some(mut col) = group.next_column()? {
        match columns.next().expect("one value list per column") {
            Column::Text(values, levels) => {
                col.typed::<ByteArrayType>().write_batch(&values, levels.as_deref(), None)?;
            }
            Column::Int32(values) => {
                col.typed::<Int32Type>().write_batch(&values, None, None)?;
            }
            Column::Int64(values) => {
                col.typed::<Int64Type>().write_batch(&values, None, None)?;
            }
        }
        col.close()?;
    }
    group.close()?;
    writer.close()?;
    Ok(())
}

pub fn write_users(path: &Path, rows: &[UserRow], category_names: &[Bytes]) -> Result<()> {
    let tops: Vec<ByteArray> =
        rows.iter().filter(|r| r.top != NO_CATEGORY).map(|r| bytes(category_names[r.top as usize])).collect();
    let levels: Vec<i16> = rows.iter().map(|r| (r.top != NO_CATEGORY) as i16).collect();
    write(
        path,
        USER_SCHEMA,
        vec![
            Column::Text(rows.iter().map(|r| bytes(r.name)).collect(), None),
            Column::Int32(rows.iter().map(|r| r.day).collect()),
            Column::Int64(rows.iter().map(|r| r.sessions as i64).collect()),
            Column::Int64(rows.iter().map(|r| r.events as i64).collect()),
            Column::Int64(rows.iter().map(|r| r.revenue).collect()),
            Column::Text(tops, Some(levels)),
        ],
    )
}

/// `counts` is indexed campaign * days + day; campaign `ids.len()` is organic.
pub fn write_campaigns(path: &Path, counts: &[CampaignDay], d: &Dims, ids: &[Bytes], channels: &[Bytes]) -> Result<()> {
    let (mut id, mut channel, mut day) = (Vec::new(), Vec::new(), Vec::new());
    let mut stats: [Vec<i64>; 4] = Default::default();
    for (c, chunk) in counts.chunks(d.days).enumerate() {
        for (di, cd) in chunk.iter().enumerate() {
            if cd[0] == 0 && cd[3] == 0 {
                continue;
            }
            let (name, chan): (&[u8], &[u8]) = if c == ids.len() { (b"organic", b"organic") } else { (ids[c], channels[c]) };
            id.push(bytes(name));
            channel.push(bytes(chan));
            day.push((d.first_day + di as i64) as i32);
            for k in 0..4 {
                stats[k].push(cd[k]);
            }
        }
    }
    let [sessions, purchasing, revenue, assisted] = stats;
    write(
        path,
        CAMPAIGN_SCHEMA,
        vec![
            Column::Text(id, None),
            Column::Text(channel, None),
            Column::Int32(day),
            Column::Int64(sessions),
            Column::Int64(purchasing),
            Column::Int64(revenue),
            Column::Int64(assisted),
        ],
    )
}
