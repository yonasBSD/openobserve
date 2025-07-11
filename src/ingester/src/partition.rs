// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{collections::BTreeMap, fs::create_dir_all, path::PathBuf, sync::Arc};

use arrow_schema::Schema;
use config::{
    meta::stream::FileMeta,
    metrics,
    utils::{
        parquet::{generate_filename_with_time_range, new_parquet_writer},
        record_batch_ext::merge_record_batches,
        schema::filter_source_by_partition_key,
        schema_ext::SchemaExt,
    },
};
use hashbrown::HashSet;
use snafu::ResultExt;
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

use crate::{
    ReadRecordBatchEntry,
    entry::{Entry, PersistStat, RecordBatchEntry},
    errors::*,
};

pub(crate) struct Partition {
    schema: Arc<Schema>,
    schema_fields: HashSet<String>, // use for quick check schema change
    files: BTreeMap<Arc<str>, PartitionFile>, // key: hour, val: files
}

impl Partition {
    pub(crate) fn new() -> Self {
        Self {
            schema: Arc::new(Schema::empty()),
            schema_fields: HashSet::new(),
            files: BTreeMap::default(),
        }
    }

    pub(crate) fn write(&mut self, entry: Entry, batch: Arc<RecordBatchEntry>) -> Result<usize> {
        let partition = self
            .files
            .entry(entry.partition_key.clone())
            .or_insert_with(PartitionFile::new);

        let schema = batch.data.schema();
        let mut old_schema_size = self.schema.size();
        if self.schema_fields.is_empty() {
            self.schema = schema.clone();
            self.schema_fields = self
                .schema
                .fields()
                .iter()
                .map(|f| f.name().to_string())
                .collect();
            // old schema is empty
            old_schema_size = 0;
        } else {
            let new_fields = schema
                .fields()
                .iter()
                .filter_map(|f| {
                    if !self.schema_fields.contains(f.name()) {
                        Some(f.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            if !new_fields.is_empty() {
                self.schema_fields
                    .extend(new_fields.iter().map(|f| f.name().to_string()));
                let schema_fields = self
                    .schema
                    .fields()
                    .iter()
                    .cloned()
                    .chain(new_fields)
                    .collect::<Vec<_>>();
                self.schema = Arc::new(Schema::new(schema_fields));
            }
        }

        let new_schema_size = self.schema.size();
        match new_schema_size.cmp(&old_schema_size) {
            std::cmp::Ordering::Greater => {
                let diff = new_schema_size - old_schema_size;
                metrics::INGEST_MEMTABLE_ARROW_BYTES
                    .with_label_values(&[])
                    .add(diff as i64);
            }
            std::cmp::Ordering::Less => {
                let diff = old_schema_size - new_schema_size;
                metrics::INGEST_MEMTABLE_ARROW_BYTES
                    .with_label_values(&[])
                    .sub(diff as i64);
            }
            std::cmp::Ordering::Equal => {}
        }

        partition.write(batch)
    }

    pub(crate) fn read(
        &self,
        time_range: Option<(i64, i64)>,
        partition_filters: &[(String, Vec<String>)],
    ) -> Result<ReadRecordBatchEntry> {
        let mut batches = Vec::with_capacity(self.files.len());
        for (key, file) in self.files.iter() {
            let key = format!("{key}/");
            if filter_source_by_partition_key(&key, partition_filters) {
                batches.extend(file.read(time_range)?);
            } else {
                log::debug!("memtable skip key: {key:?}");
            }
        }
        Ok((self.schema.clone(), batches))
    }

    pub(crate) async fn persist(
        &self,
        idx: usize,
        org_id: &str,
        stream_type: &str,
        stream_name: &str,
    ) -> Result<(usize, Vec<(PathBuf, PersistStat)>)> {
        let cfg = config::get_config();
        let base_path = PathBuf::from(&cfg.common.data_wal_dir);
        let mut path = base_path.clone();
        path.push("files");
        path.push(org_id);
        path.push(stream_type);
        path.push(stream_name);
        path.push(idx.to_string());
        let mut paths = Vec::with_capacity(self.files.len());
        for (hour, data) in self.files.iter() {
            if data.data.is_empty() {
                continue;
            }
            let mut chunks = Vec::new();
            let mut cur_batches = Vec::new();
            let mut cur_num_rows = 0;
            for data in data.data.iter() {
                let num_rows = data.data.num_rows();
                if cur_num_rows > 0 && cur_num_rows + num_rows > config::PARQUET_FILE_CHUNK_SIZE {
                    chunks.push(cur_batches);
                    cur_num_rows = 0;
                    cur_batches = Vec::new();
                }
                cur_num_rows += num_rows;
                cur_batches.push(data);
            }
            if !cur_batches.is_empty() {
                chunks.push(cur_batches);
            }
            for data in chunks {
                let mut file_meta = FileMeta::default();
                data.iter().for_each(|r| {
                    file_meta.original_size += r.data_json_size as i64;
                    file_meta.records += r.data.num_rows() as i64;
                    if file_meta.min_ts == 0 || file_meta.min_ts > r.min_ts {
                        file_meta.min_ts = r.min_ts;
                    }
                    if file_meta.max_ts < r.max_ts {
                        file_meta.max_ts = r.max_ts;
                    }
                });
                let mut persist_stat = PersistStat {
                    json_size: file_meta.original_size,
                    arrow_size: 0,
                    file_num: 1,
                    batch_num: data.len(),
                };
                // write into parquet buf
                let bloom_filter_fields =
                    if self.schema.fields().len() >= cfg.limit.file_move_fields_limit {
                        let settings = infra::schema::unwrap_stream_settings(self.schema.as_ref());
                        infra::schema::get_stream_setting_bloom_filter_fields(&settings)
                    } else {
                        vec![]
                    };

                let batches = data
                    .iter()
                    .map(|r| {
                        persist_stat.arrow_size += r.data_arrow_size;
                        r.data.clone()
                    })
                    .collect::<Vec<_>>();

                let (schema, batches) =
                    merge_record_batches("INGESTER:PERSIST", 0, self.schema.clone(), batches)
                        .context(MergeRecordBatchSnafu)?;

                let mut buf_parquet = Vec::new();
                let compression = if cfg.common.feature_ingester_none_compression {
                    Some("none")
                } else {
                    None
                };
                let mut writer = new_parquet_writer(
                    &mut buf_parquet,
                    &schema,
                    &bloom_filter_fields,
                    &file_meta,
                    true,
                    compression,
                );

                writer
                    .write(&batches)
                    .await
                    .context(WriteParquetRecordBatchSnafu)?;
                writer.close().await.context(WriteParquetRecordBatchSnafu)?;
                file_meta.compressed_size = buf_parquet.len() as i64;

                // write into local file
                let file_name =
                    generate_filename_with_time_range(file_meta.min_ts, file_meta.max_ts);
                let mut path = path.clone();
                path.push(hour.to_string());
                path.push(file_name);
                path.set_extension("par");
                create_dir_all(path.parent().unwrap())
                    .context(CreateFileSnafu { path: path.clone() })?;
                let mut f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .await
                    .context(CreateFileSnafu { path: path.clone() })?;
                f.write_all(&buf_parquet)
                    .await
                    .context(WriteFileSnafu { path: path.clone() })?;
                drop(f);

                // set parquet metadata cache
                let mut file_key = path.clone();
                file_key.set_extension("parquet");
                let file_key = file_key
                    .strip_prefix(base_path.clone())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
                    .trim_start_matches('/')
                    .to_string();
                super::WAL_PARQUET_METADATA
                    .write()
                    .await
                    .insert(file_key, file_meta);

                // update metrics
                metrics::INGEST_WAL_USED_BYTES
                    .with_label_values(&[org_id, stream_type])
                    .add(buf_parquet.len() as i64);
                metrics::INGEST_WAL_WRITE_BYTES
                    .with_label_values(&[org_id, stream_type])
                    .inc_by(buf_parquet.len() as u64);

                paths.push((path, persist_stat));
            }
        }
        Ok((self.schema.size(), paths))
    }
}

struct PartitionFile {
    data: Vec<Arc<RecordBatchEntry>>,
}

impl PartitionFile {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn write(&mut self, batch: Arc<RecordBatchEntry>) -> Result<usize> {
        let json_size = batch.data_json_size;
        let arrow_size = batch.data_arrow_size;
        self.data.push(batch);
        metrics::INGEST_MEMTABLE_ARROW_BYTES
            .with_label_values(&[])
            .add(arrow_size as i64);
        metrics::INGEST_MEMTABLE_BYTES
            .with_label_values(&[])
            .add(json_size as i64);
        Ok(arrow_size)
    }

    fn read(&self, time_range: Option<(i64, i64)>) -> Result<Vec<Arc<RecordBatchEntry>>> {
        match time_range {
            None | Some((0, 0)) => Ok(self.data.clone()),
            Some((min_ts, max_ts)) => Ok(self
                .data
                .iter()
                .filter(|r| r.min_ts <= max_ts && r.max_ts >= min_ts)
                .cloned()
                .collect()),
        }
    }
}
