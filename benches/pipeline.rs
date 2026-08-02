use std::hint::black_box;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use escom::formatting::{DisplayFormatter, FormattedRow, write_export};
use escom::model::{ReceiveMode, TextEncoding};
use escom::search::{SearchDisplayOptions, search_rows};
use escom::store::{ReceiveDelta, ReceiveSnapshot, RxChunk};

const MIB: usize = 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const SEARCH_ROWS: usize = 100_000;
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

fn benchmark_sizes_mib() -> &'static [usize] {
    if std::env::var_os("ESCOM_BENCH_LARGE").is_some() {
        &[20, 100, 500]
    } else {
        &[20]
    }
}

fn payload() -> Arc<[u8]> {
    let line = b"INFO sensor=temperature value=23.75 status=nominal sequence=0123456789\r\n";
    let mut bytes = Vec::with_capacity(CHUNK_BYTES);
    while bytes.len() < CHUNK_BYTES {
        let remaining = CHUNK_BYTES - bytes.len();
        bytes.extend_from_slice(&line[..line.len().min(remaining)]);
    }
    bytes.into()
}

fn snapshot(mebibytes: usize) -> ReceiveSnapshot {
    let bytes_len = mebibytes * MIB;
    let payload = payload();
    let received_at = Local::now();
    let chunk_count = bytes_len.div_ceil(payload.len());
    let mut chunks = Vec::with_capacity(chunk_count);

    for index in 0..chunk_count {
        let offset = index * payload.len();
        let remaining = bytes_len - offset;
        let bytes = if remaining >= payload.len() {
            Arc::clone(&payload)
        } else {
            Arc::from(&payload[..remaining])
        };
        chunks.push(RxChunk {
            sequence: index as u64,
            received_at,
            bytes,
        });
    }

    ReceiveSnapshot {
        generation: 1,
        stream_id: 1,
        first_sequence: 0,
        next_sequence: chunks.len() as u64,
        chunks,
        bytes_len,
        omitted_bytes: 0,
        dropped_bytes: 0,
    }
}

fn appended_delta(base: &ReceiveSnapshot, bytes_len: usize) -> ReceiveDelta {
    let payload = payload();
    let received_at = Local::now();
    let chunk_count = bytes_len.div_ceil(payload.len());
    let mut chunks = Vec::with_capacity(chunk_count);
    for index in 0..chunk_count {
        let remaining = bytes_len - index * payload.len();
        chunks.push(RxChunk {
            sequence: base.next_sequence + index as u64,
            received_at,
            bytes: if remaining >= payload.len() {
                Arc::clone(&payload)
            } else {
                Arc::from(&payload[..remaining])
            },
        });
    }
    ReceiveDelta {
        generation: base.generation + 1,
        stream_id: base.stream_id,
        first_sequence: base.first_sequence,
        next_sequence: base.next_sequence + chunks.len() as u64,
        chunks,
        reset_or_gap: false,
    }
}

fn search_fixture() -> Vec<FormattedRow> {
    let received_at = Local::now();
    (0..SEARCH_ROWS)
        .map(|index| FormattedRow {
            received_at,
            text: if index % 97 == 0 {
                format!("ERROR device response timed out request={index}")
            } else {
                format!("INFO device response accepted request={index}")
            },
        })
        .collect()
}

fn display_rebuild_benchmark(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("display_rebuild_text");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    for &mebibytes in benchmark_sizes_mib() {
        let input = snapshot(mebibytes);
        group.throughput(Throughput::Bytes(input.bytes_len as u64));
        group.bench_with_input(
            BenchmarkId::new("buffer_mib", mebibytes),
            &input,
            |benchmark, snapshot| {
                benchmark.iter(|| {
                    let (formatter, rows) = DisplayFormatter::rebuild(
                        black_box(snapshot),
                        ReceiveMode::Text,
                        TextEncoding::Utf8,
                    );
                    black_box((formatter.cursor(), rows.len()));
                });
            },
        );
    }
    group.finish();
}

fn display_delta_benchmark(criterion: &mut Criterion) {
    let base = snapshot(1);
    let delta_bytes = 128 * 1024;
    let delta = appended_delta(&base, delta_bytes);
    let mut group = criterion.benchmark_group("display_delta_text");
    group.throughput(Throughput::Bytes(delta_bytes as u64));
    group.bench_function("append_128_kib", |benchmark| {
        benchmark.iter_batched(
            || DisplayFormatter::rebuild(&base, ReceiveMode::Text, TextEncoding::Utf8).0,
            |mut formatter| black_box(formatter.apply_delta(black_box(&delta)).unwrap()),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn search_benchmark(criterion: &mut Criterion) {
    let rows = search_fixture();
    let options = SearchDisplayOptions::new(false, TIMESTAMP_FORMAT);
    let mut group = criterion.benchmark_group("search_100k_rows");
    group.throughput(Throughput::Elements(rows.len() as u64));
    for (name, query, regex) in [
        ("literal", "ERROR", false),
        ("regex", r"ERROR.*request=\d+", true),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(query, regex),
            |benchmark, &(query, regex)| {
                benchmark.iter(|| {
                    black_box(search_rows(
                        black_box(&rows),
                        black_box(query),
                        false,
                        regex,
                        options,
                    ))
                });
            },
        );
    }
    group.finish();
}

fn export_benchmark(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("streaming_export_text");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    for &mebibytes in benchmark_sizes_mib() {
        let input = snapshot(mebibytes);
        group.throughput(Throughput::Bytes(input.bytes_len as u64));
        group.bench_with_input(
            BenchmarkId::new("buffer_mib", mebibytes),
            &input,
            |benchmark, snapshot| {
                benchmark.iter_batched(
                    || snapshot.clone(),
                    |snapshot| {
                        let mut sink = io::sink();
                        write_export(
                            &mut sink,
                            snapshot,
                            ReceiveMode::Text,
                            TextEncoding::Utf8,
                            false,
                            TIMESTAMP_FORMAT,
                        )
                        .unwrap();
                        black_box(sink);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    display_rebuild_benchmark,
    display_delta_benchmark,
    search_benchmark,
    export_benchmark
);
criterion_main!(benches);
