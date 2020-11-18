// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::mem;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use kvproto::coprocessor::{KeyRange, Response};
use protobuf::Message;
use rand::rngs::StdRng;
use rand::Rng;
use tidb_query_common::storage::scanner::{RangesScanner, RangesScannerOptions};
use tidb_query_common::storage::Range;
use tidb_query_datatype::codec::datum::{encode_value, split_datum, Datum, NIL_FLAG};
use tidb_query_datatype::codec::table;
use tidb_query_datatype::def::Collation;
use tidb_query_datatype::expr::{EvalConfig, EvalContext};
use tidb_query_datatype::FieldTypeAccessor;
use tidb_query_executors::{
    interface::BatchExecutor, runner::MAX_TIME_SLICE, BatchTableScanExecutor,
};
use tidb_query_expr::BATCH_MAX_SIZE;
use tipb::{self, AnalyzeColumnsReq, AnalyzeIndexReq, AnalyzeReq, AnalyzeType};
use yatp::task::future::reschedule;

use super::cmsketch::CmSketch;
use super::fmsketch::FmSketch;
use super::histogram::Histogram;
use crate::coprocessor::dag::TiKVStorage;
use crate::coprocessor::*;
use crate::storage::{Snapshot, SnapshotStore, Statistics};

// `AnalyzeContext` is used to handle `AnalyzeReq`
pub struct AnalyzeContext<S: Snapshot> {
    req: AnalyzeReq,
    storage: Option<TiKVStorage<SnapshotStore<S>>>,
    ranges: Vec<KeyRange>,
    storage_stats: Statistics,
}

impl<S: Snapshot> AnalyzeContext<S> {
    pub fn new(
        req: AnalyzeReq,
        ranges: Vec<KeyRange>,
        start_ts: u64,
        snap: S,
        req_ctx: &ReqContext,
    ) -> Result<Self> {
        let store = SnapshotStore::new(
            snap,
            start_ts.into(),
            req_ctx.context.get_isolation_level(),
            !req_ctx.context.get_not_fill_cache(),
            req_ctx.bypass_locks.clone(),
            false,
        );
        Ok(Self {
            req,
            storage: Some(TiKVStorage::new(store, false)),
            ranges,
            storage_stats: Statistics::default(),
        })
    }

    // handle_column is used to process `AnalyzeColumnsReq`
    // it would build a histogram for the primary key(if needed) and
    // collectors for each column value.
    async fn handle_column(builder: &mut SampleBuilder<S>) -> Result<Vec<u8>> {
        let (collectors, pk_builder) = builder.collect_columns_stats().await?;

        let pk_hist = pk_builder.into_proto();
        let cols: Vec<tipb::SampleCollector> =
            collectors.into_iter().map(|col| col.into_proto()).collect();

        let res_data = {
            let mut res = tipb::AnalyzeColumnsResp::default();
            res.set_collectors(cols.into());
            res.set_pk_hist(pk_hist);
            box_try!(res.write_to_bytes())
        };
        Ok(res_data)
    }

    // handle_index is used to handle `AnalyzeIndexReq`,
    // it would build a histogram and count-min sketch of index values.
    async fn handle_index(
        req: AnalyzeIndexReq,
        scanner: &mut RangesScanner<TiKVStorage<SnapshotStore<S>>>,
        is_common_handle: bool,
    ) -> Result<Vec<u8>> {
        let mut hist = Histogram::new(req.get_bucket_size() as usize);
        let mut cms = CmSketch::new(
            req.get_cmsketch_depth() as usize,
            req.get_cmsketch_width() as usize,
        );

        let mut row_count = 0;
        let mut time_slice_start = Instant::now();
        while let Some((key, _)) = scanner.next()? {
            row_count += 1;
            if row_count >= BATCH_MAX_SIZE {
                if time_slice_start.elapsed() > MAX_TIME_SLICE {
                    reschedule().await;
                    time_slice_start = Instant::now();
                }
                row_count = 0;
            }
            let mut key = &key[..];
            if is_common_handle {
                table::check_record_key(key)?;
                key = &key[table::PREFIX_LEN..];
            } else {
                table::check_index_key(key)?;
                key = &key[table::PREFIX_LEN + table::ID_LEN..];
            }
            let mut datums = key;
            let mut data = Vec::with_capacity(key.len());
            for i in 0..req.get_num_columns() as usize {
                if datums.is_empty() {
                    return Err(box_err!(
                        "{}th column is missing in datum buffer: {}",
                        i,
                        hex::encode_upper(key)
                    ));
                }
                let (column, remaining) = split_datum(datums, false)?;
                datums = remaining;
                data.extend_from_slice(column);
                if let Some(cms) = cms.as_mut() {
                    cms.insert(&data);
                }
            }
            hist.append(&data);
        }

        let mut res = tipb::AnalyzeIndexResp::default();
        res.set_hist(hist.into_proto());
        if let Some(c) = cms {
            res.set_cms(c.into_proto());
        }
        let dt = box_try!(res.write_to_bytes());
        Ok(dt)
    }
}

#[async_trait]
impl<S: Snapshot> RequestHandler for AnalyzeContext<S> {
    async fn handle_request(&mut self) -> Result<Response> {
        let ret = match self.req.get_tp() {
            AnalyzeType::TypeIndex | AnalyzeType::TypeCommonHandle => {
                let req = self.req.take_idx_req();
                let ranges = mem::replace(&mut self.ranges, vec![]);
                table::check_table_ranges(&ranges)?;
                let mut scanner = RangesScanner::new(RangesScannerOptions {
                    storage: self.storage.take().unwrap(),
                    ranges: ranges
                        .into_iter()
                        .map(|r| Range::from_pb_range(r, false))
                        .collect(),
                    scan_backward_in_range: false,
                    is_key_only: true,
                    is_scanned_range_aware: false,
                });
                let res = AnalyzeContext::handle_index(
                    req,
                    &mut scanner,
                    self.req.get_tp() == AnalyzeType::TypeCommonHandle,
                )
                .await;
                scanner.collect_storage_stats(&mut self.storage_stats);
                res
            }

            AnalyzeType::TypeColumn => {
                let col_req = self.req.take_col_req();
                let storage = self.storage.take().unwrap();
                let ranges = mem::replace(&mut self.ranges, Vec::new());
                let mut builder = SampleBuilder::new(col_req, storage, ranges)?;
                let res = AnalyzeContext::handle_column(&mut builder).await;
                builder.data.collect_storage_stats(&mut self.storage_stats);
                res
            }
        };
        match ret {
            Ok(data) => {
                let mut resp = Response::default();
                resp.set_data(data);
                Ok(resp)
            }
            Err(Error::Other(e)) => {
                let mut resp = Response::default();
                resp.set_other_error(e);
                Ok(resp)
            }
            Err(e) => Err(e),
        }
    }

    fn collect_scan_statistics(&mut self, dest: &mut Statistics) {
        dest.add(&self.storage_stats);
        self.storage_stats = Statistics::default();
    }
}

struct SampleBuilder<S: Snapshot> {
    data: BatchTableScanExecutor<TiKVStorage<SnapshotStore<S>>>,

    max_bucket_size: usize,
    max_sample_size: usize,
    max_fm_sketch_size: usize,
    cm_sketch_depth: usize,
    cm_sketch_width: usize,
    columns_info: Vec<tipb::ColumnInfo>,
}

/// `SampleBuilder` is used to analyze columns. It collects sample from
/// the result set using Reservoir Sampling algorithm, estimates NDVs
/// using FM Sketch during the collecting process, and builds count-min sketch.
impl<S: Snapshot> SampleBuilder<S> {
    fn new(
        mut req: AnalyzeColumnsReq,
        storage: TiKVStorage<SnapshotStore<S>>,
        ranges: Vec<KeyRange>,
    ) -> Result<Self> {
        let columns_info: Vec<_> = req.take_columns_info().into();
        if columns_info.is_empty() {
            return Err(box_err!("empty columns_info"));
        }

        let table_scanner = BatchTableScanExecutor::new(
            storage,
            Arc::new(EvalConfig::default()),
            columns_info.clone(),
            ranges,
            req.take_primary_column_ids(),
            false,
            false, // Streaming mode is not supported in Analyze request, always false here
        )?;
        Ok(Self {
            data: table_scanner,
            max_bucket_size: req.get_bucket_size() as usize,
            max_fm_sketch_size: req.get_sketch_size() as usize,
            max_sample_size: req.get_sample_size() as usize,
            cm_sketch_depth: req.get_cmsketch_depth() as usize,
            cm_sketch_width: req.get_cmsketch_width() as usize,
            columns_info,
        })
    }

    // `collect_columns_stats` returns the sample collectors which contain total count,
    // null count, distinct values count and count-min sketch. And it also returns the statistic
    // builder for PK which contains the histogram.
    // See https://en.wikipedia.org/wiki/Reservoir_sampling
    async fn collect_columns_stats(&mut self) -> Result<(Vec<SampleCollector>, Histogram)> {
        use tidb_query_datatype::codec::collation::{match_template_collator, Collator};
        let columns_without_handle_len =
            self.columns_info.len() - self.columns_info[0].get_pk_handle() as usize;

        // The number of columns need to be sampled is `columns_without_handle_len`.
        // It equals to `columns_info.len()` if the first column doesn't contain a handle.
        // Otherwise, it equals to `columns_info.len() - 1`.
        let mut pk_builder = Histogram::new(self.max_bucket_size);
        let mut collectors = vec![
            SampleCollector::new(
                self.max_sample_size,
                self.max_fm_sketch_size,
                self.cm_sketch_depth,
                self.cm_sketch_width,
            );
            columns_without_handle_len
        ];
        let mut is_drained = false;
        let mut time_slice_start = Instant::now();
        while !is_drained {
            let time_slice_elapsed = time_slice_start.elapsed();
            if time_slice_elapsed > MAX_TIME_SLICE {
                reschedule().await;
                time_slice_start = Instant::now();
            }
            let result = self.data.next_batch(BATCH_MAX_SIZE);
            is_drained = result.is_drained?;

            let mut columns_slice = result.physical_columns.as_slice();
            let mut columns_info = &self.columns_info[..];
            if columns_without_handle_len + 1 == columns_slice.len() {
                for logical_row in &result.logical_rows {
                    let mut data = vec![];
                    columns_slice[0].encode(
                        *logical_row,
                        &columns_info[0],
                        &mut EvalContext::default(),
                        &mut data,
                    )?;
                    pk_builder.append(&data);
                }
                columns_slice = &columns_slice[1..];
                columns_info = &columns_info[1..];
            }

            for (i, collector) in collectors.iter_mut().enumerate() {
                for logical_row in &result.logical_rows {
                    let mut val = vec![];
                    columns_slice[i].encode(
                        *logical_row,
                        &columns_info[i],
                        &mut EvalContext::default(),
                        &mut val,
                    )?;
                    if columns_info[i].as_accessor().is_string_like() {
                        let sorted_val = match_template_collator! {
                            TT, match columns_info[i].as_accessor().collation()? {
                                Collation::TT => {
                                    let mut mut_val = &val[..];
                                    let decoded_val = table::decode_col_value(&mut mut_val, &mut EvalContext::default(), &columns_info[i])?;
                                    if decoded_val == Datum::Null {
                                        val
                                    } else {
                                        // Only if the `decoded_val` is Datum::Null, `decoded_val` is a Ok(None).
                                        // So it is safe the unwrap the Ok value.
                                        let decoded_sorted_val = TT::sort_key(&decoded_val.as_string()?.unwrap().into_owned())?;
                                        encode_value(&mut EvalContext::default(), &[Datum::Bytes(decoded_sorted_val)])?
                                    }
                                }
                            }
                        };
                        collector.collect(sorted_val);
                        continue;
                    }
                    collector.collect(val);
                }
            }
        }
        Ok((collectors, pk_builder))
    }
}

/// `SampleCollector` will collect Samples and calculate the count, ndv and total size of an attribute.
#[derive(Clone)]
struct SampleCollector {
    samples: Vec<Vec<u8>>,
    null_count: u64,
    count: u64,
    max_sample_size: usize,
    fm_sketch: FmSketch,
    cm_sketch: Option<CmSketch>,
    rng: StdRng,
    total_size: u64,
}

impl SampleCollector {
    fn new(
        max_sample_size: usize,
        max_fm_sketch_size: usize,
        cm_sketch_depth: usize,
        cm_sketch_width: usize,
    ) -> SampleCollector {
        SampleCollector {
            samples: Default::default(),
            null_count: 0,
            count: 0,
            max_sample_size,
            fm_sketch: FmSketch::new(max_fm_sketch_size),
            cm_sketch: CmSketch::new(cm_sketch_depth, cm_sketch_width),
            rng: StdRng::from_entropy(),
            total_size: 0,
        }
    }

    fn into_proto(self) -> tipb::SampleCollector {
        let mut s = tipb::SampleCollector::default();
        s.set_null_count(self.null_count as i64);
        s.set_count(self.count as i64);
        s.set_fm_sketch(self.fm_sketch.into_proto());
        s.set_samples(self.samples.into());
        if let Some(c) = self.cm_sketch {
            s.set_cm_sketch(c.into_proto())
        }
        s.set_total_size(self.total_size as i64);
        s
    }

    pub fn collect(&mut self, data: Vec<u8>) {
        if data[0] == NIL_FLAG {
            self.null_count += 1;
            return;
        }
        self.count += 1;
        self.fm_sketch.insert(&data);
        if let Some(c) = self.cm_sketch.as_mut() {
            c.insert(&data);
        }
        self.total_size += data.len() as u64;
        if self.samples.len() < self.max_sample_size {
            self.samples.push(data);
            return;
        }
        if self.rng.gen_range(0, self.count) < self.max_sample_size as u64 {
            let idx = self.rng.gen_range(0, self.max_sample_size);
            // https://github.com/pingcap/tidb/blob/master/statistics/sample.go#L173
            self.samples.remove(idx);
            self.samples.push(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tidb_query_datatype::codec::datum;
    use tidb_query_datatype::codec::datum::Datum;

    #[test]
    fn test_sample_collector() {
        let max_sample_size = 3;
        let max_fm_sketch_size = 10;
        let cm_sketch_depth = 2;
        let cm_sketch_width = 16;
        let mut sample = SampleCollector::new(
            max_sample_size,
            max_fm_sketch_size,
            cm_sketch_depth,
            cm_sketch_width,
        );
        let cases = vec![Datum::I64(1), Datum::Null, Datum::I64(2), Datum::I64(5)];

        for data in cases {
            sample.collect(datum::encode_value(&mut EvalContext::default(), &[data]).unwrap());
        }
        assert_eq!(sample.samples.len(), max_sample_size);
        assert_eq!(sample.null_count, 1);
        assert_eq!(sample.count, 3);
        assert_eq!(sample.cm_sketch.unwrap().count(), 3);
        assert_eq!(sample.total_size, 6)
    }
}
