//! What RookDB's tuple format buys when rows carry variable-length columns.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin tuple_bench
//!   ./target/release/tuple_bench [rows] [repeats]
//!
//! RookDB stores a row as
//!
//! ```text
//! [header 4B][null bitmap][var-len offset table][fixed-length data][var-len data]
//! ```
//!
//! with every fixed-length column moved ahead of the var-len ones, so any
//! fixed column sits at an offset known from the schema alone, and any
//! var-len column is one offset-table read away.
//!
//! The baseline here is the conventional alternative: columns in declared
//! order, each var-len value preceded by a u16 length. Reaching column `k`
//! means walking every column before it. Both formats decode values with
//! the same `DataValue::from_bytes`, so only the layout differs.
//!
//! Part 1 measures single rows in memory (no I/O):
//!   * reading the fixed column that comes after `k` VARCHARs, k = 0..8;
//!   * reading the last VARCHAR;
//!   * decoding every column;
//!   * bytes per row.
//!
//! Part 2 runs SQL on a table of the same shape through the engine, with
//! scans decoding only the columns a query reads (the default) and with
//! `set_decode_all_columns(true)`, which decodes every column.
//!
//! Each figure is the fastest of `repeats` runs (default 7).

use std::hint::black_box;
use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};
use storage_manager::types::{
    DataType, DataValue, PhysicalSchema, RowDecoder, RowLayout, serialize_nullable_typed_row,
};

/// VARCHAR columns per row.
const VARLEN_COLS: usize = 8;

/// The logical schema: `f0 v0 f1 v1 … f7 v7 f8`, a BIGINT ahead of each
/// VARCHAR and one at the end, so fixed column `f_k` (logical index `2k`)
/// comes after exactly `k` VARCHARs.
fn schema() -> Vec<DataType> {
    let mut s = Vec::new();
    for _ in 0..VARLEN_COLS {
        s.push(DataType::BigInt);
        s.push(DataType::Varchar(64));
    }
    s.push(DataType::BigInt);
    s
}

fn fixed_col(k: usize) -> usize {
    2 * k
}

fn varlen_col(k: usize) -> usize {
    2 * k + 1
}

/// A small deterministic generator, so runs are comparable.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn make_rows(n: usize) -> Vec<Vec<Option<DataValue>>> {
    let mut rng = Lcg(42);
    (0..n)
        .map(|i| {
            let mut row = Vec::with_capacity(2 * VARLEN_COLS + 1);
            for c in 0..VARLEN_COLS {
                row.push(Some(DataValue::BigInt((i * 31 + c) as i64)));
                let len = 16 + (rng.next() % 41) as usize; // 16..=56 bytes
                let s: String = (0..len)
                    .map(|j| (b'a' + ((i + j + c) % 26) as u8) as char)
                    .collect();
                row.push(Some(DataValue::Varchar(s)));
            }
            row.push(Some(DataValue::BigInt(i as i64)));
            row
        })
        .collect()
}

// ── Baseline format ───────────────────────────────────────────────────────────
//
// [null bitmap][col 0][col 1]…  in declared order; a fixed column is its
// bytes, a var-len column is [u16 length][bytes].

struct Baseline {
    schema: Vec<DataType>,
    sizes: Vec<Option<usize>>,
    bitmap: usize,
}

impl Baseline {
    fn new(schema: &[DataType]) -> Self {
        Baseline {
            schema: schema.to_vec(),
            sizes: schema
                .iter()
                .map(|t| {
                    t.is_fixed_length()
                        .then(|| t.fixed_size().unwrap() as usize)
                })
                .collect(),
            bitmap: schema.len().div_ceil(8),
        }
    }

    fn encode(&self, row: &[Option<DataValue>]) -> Vec<u8> {
        let mut out = vec![0u8; self.bitmap];
        for (i, v) in row.iter().enumerate() {
            match v {
                None => out[i / 8] |= 1 << (i % 8),
                Some(v) => {
                    let bytes = v.to_bytes();
                    if self.sizes[i].is_none() {
                        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                    }
                    out.extend_from_slice(&bytes);
                }
            }
        }
        out
    }

    fn is_null(&self, row: &[u8], i: usize) -> bool {
        row[i / 8] & (1 << (i % 8)) != 0
    }

    /// Byte range of column `target`, found by walking the columns before it.
    fn locate(&self, row: &[u8], target: usize) -> Option<(usize, usize)> {
        if self.is_null(row, target) {
            return None;
        }
        let mut pos = self.bitmap;
        for i in 0..target {
            if self.is_null(row, i) {
                continue;
            }
            pos += match self.sizes[i] {
                Some(size) => size,
                None => 2 + u16::from_le_bytes([row[pos], row[pos + 1]]) as usize,
            };
        }
        Some(match self.sizes[target] {
            Some(size) => (pos, pos + size),
            None => {
                let len = u16::from_le_bytes([row[pos], row[pos + 1]]) as usize;
                (pos + 2, pos + 2 + len)
            }
        })
    }

    fn read(&self, row: &[u8], col: usize) -> Option<DataValue> {
        let (start, end) = self.locate(row, col)?;
        Some(DataValue::from_bytes(&self.schema[col], &row[start..end]).unwrap())
    }

    fn decode_into(&self, row: &[u8], out: &mut Vec<Option<DataValue>>) {
        out.clear();
        let mut pos = self.bitmap;
        for (i, ty) in self.schema.iter().enumerate() {
            if self.is_null(row, i) {
                out.push(None);
                continue;
            }
            let (start, end) = match self.sizes[i] {
                Some(size) => (pos, pos + size),
                None => {
                    let len = u16::from_le_bytes([row[pos], row[pos + 1]]) as usize;
                    (pos + 2, pos + 2 + len)
                }
            };
            out.push(Some(DataValue::from_bytes(ty, &row[start..end]).unwrap()));
            pos = end;
        }
    }
}

// ── RookDB format, read directly ──────────────────────────────────────────────
//
// Where a column is, worked out once from the schema with the format's own
// `PhysicalSchema` / `RowLayout`: what the layout promises, without the
// engine decoder's full-width output row around it.

enum Slot {
    Fixed { start: usize, end: usize },
    Varlen { rank: usize },
}

struct RookColumn {
    ty: DataType,
    col: usize,
    slot: Slot,
    varlen_table: usize,
    num_varlen: usize,
}

impl RookColumn {
    fn new(schema: &[DataType], col: usize) -> Self {
        let physical = PhysicalSchema::from_logical(schema);
        let layout = RowLayout::compute(&physical);
        let phys = physical.logical_to_physical[col];
        let slot = if schema[col].is_fixed_length() {
            let start = layout.fixed_data_start + layout.fixed_col_offsets[phys];
            Slot::Fixed {
                start,
                end: start + schema[col].fixed_size().unwrap() as usize,
            }
        } else {
            Slot::Varlen {
                rank: phys - physical.num_fixed(),
            }
        };
        RookColumn {
            ty: schema[col].clone(),
            col,
            slot,
            varlen_table: layout.varlen_table_offset(),
            num_varlen: physical.num_varlen(),
        }
    }

    fn read(&self, row: &[u8]) -> Option<DataValue> {
        if row[4 + self.col / 8] & (1 << (self.col % 8)) != 0 {
            return None;
        }
        let (start, end) = match self.slot {
            Slot::Fixed { start, end } => (start, end),
            Slot::Varlen { rank } => {
                let at = |r: usize| {
                    let o = self.varlen_table + 2 * r;
                    u16::from_le_bytes([row[o], row[o + 1]]) as usize
                };
                let end = (rank + 1..self.num_varlen)
                    .map(at)
                    .find(|&o| o != 0)
                    .unwrap_or(row.len());
                (at(rank), end)
            }
        };
        Some(DataValue::from_bytes(&self.ty, &row[start..end]).unwrap())
    }
}

// ── Timing ────────────────────────────────────────────────────────────────────

/// Fastest of `repeats` runs of `f`, in nanoseconds per row read; `f` reads
/// `reads` rows.
fn ns_per_row(repeats: usize, reads: usize, mut f: impl FnMut()) -> f64 {
    f(); // warm up
    (0..repeats)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as f64 / reads as f64
        })
        .fold(f64::INFINITY, f64::min)
}

/// Row size with fixed-length columns packed (as written now) and padded to
/// their alignment (as written before), for a table mixing narrow and wide
/// fixed columns; the benchmark table above is all BIGINT and has no padding.
fn padding_report() {
    use storage_manager::types::row_layout::PACKED_LAYOUT_FLAG;
    // An orders-like row: id, active, amount, qty, created, gift, score, note.
    let schema = vec![
        DataType::BigInt,
        DataType::Bool,
        DataType::DoublePrecision,
        DataType::SmallInt,
        DataType::Date,
        DataType::Bool,
        DataType::Int,
        DataType::Varchar(32),
    ];
    let physical = PhysicalSchema::from_logical(&schema);
    let packed = RowLayout::compute(&physical);
    let padded = RowLayout::compute_padded(&physical);
    let bytes = storage_manager::types::serialize_nullable_row(
        &schema,
        &[
            Some("1"),
            Some("true"),
            Some("9.5"),
            Some("3"),
            Some("2026-01-01"),
            Some("false"),
            Some("7"),
            Some("gift wrap, leave at door"),
        ],
    )
    .unwrap();
    assert_ne!(
        u16::from_le_bytes([bytes[2], bytes[3]]) & PACKED_LAYOUT_FLAG,
        0
    );
    let now = bytes.len();
    let before = now + padded.fixed_data_size - packed.fixed_data_size;
    // A page holds rows plus an 8-byte slot each, after its 8-byte header.
    let per_page = |len: usize| (storage_manager::page::PAGE_SIZE - 8) / (len + 8);
    println!(
        "[PACKING] BIGINT BOOL DOUBLE SMALLINT DATE BOOL INT VARCHAR(24 B): \
         {} B/row padded -> {} B/row packed ({:.0}% smaller), {} -> {} rows per 8 KB page",
        before,
        now,
        100.0 * (before - now) as f64 / before as f64,
        per_page(before),
        per_page(now)
    );
}

/// Rows in the cache-resident working set (~770 KB of rows).
const HOT_ROWS: usize = 2048;

fn part1_format(rows: usize, repeats: usize) {
    let schema = schema();
    let data = make_rows(rows);
    let baseline = Baseline::new(&schema);
    let rook_rows: Vec<Vec<u8>> = data
        .iter()
        .map(|r| serialize_nullable_typed_row(&schema, r).unwrap())
        .collect();
    let base_rows: Vec<Vec<u8>> = data.iter().map(|r| baseline.encode(r)).collect();

    // Every reader must give back what was stored.
    let full = RowDecoder::new(&schema);
    let mut a = Vec::new();
    let mut b = Vec::new();
    for (i, (r, bl)) in rook_rows.iter().zip(&base_rows).enumerate().step_by(997) {
        full.decode_into(r, &mut a).unwrap();
        baseline.decode_into(bl, &mut b);
        assert_eq!(a, data[i], "RookDB format round trip");
        assert_eq!(b, data[i], "baseline format round trip");
        for col in 0..schema.len() {
            assert_eq!(RookColumn::new(&schema, col).read(r), data[i][col]);
            assert_eq!(baseline.read(bl, col), data[i][col]);
        }
    }

    println!(
        "\n== Part 1: row format, in memory ({} BIGINT + {} VARCHAR(64) of 16-56 bytes per row) ==",
        VARLEN_COLS + 1,
        VARLEN_COLS
    );

    let rook_size: usize = rook_rows.iter().map(Vec::len).sum();
    let base_size: usize = base_rows.iter().map(Vec::len).sum();
    println!(
        "\n[SIZE] bytes/row: RookDB {:.1}, baseline {:.1} ({:+.1} B: RookDB's 4-byte row header; \
         its 2-byte offset per VARCHAR replaces the baseline's 2-byte length)",
        rook_size as f64 / rows as f64,
        base_size as f64 / rows as f64,
        (rook_size as f64 - base_size as f64) / rows as f64
    );

    padding_report();

    let hot = HOT_ROWS.min(rows);
    let passes = (rows / hot).max(1);
    let reads = hot * passes;
    println!(
        "\nns per row read, fastest of {} runs, lower is better. {} rows that fit in cache, read {} times.",
        repeats, hot, passes
    );
    println!("  format   = RookDB layout, column offset from the schema (RowLayout)");
    println!(
        "  decoder  = the engine's path: RowDecoder::only([col]), a full-width row of NULLs + 1 value"
    );
    println!("  baseline = declared order, [u16 len] before each VARCHAR, walk to the column\n");
    println!(
        "{:<34} {:>8} {:>8} {:>9}   {:>10}",
        "read one column", "format", "decoder", "baseline", "format vs"
    );
    println!(
        "{:<34} {:>8} {:>8} {:>9}   {:>10}",
        "", "", "", "", "baseline"
    );

    let mut out = Vec::with_capacity(schema.len());
    let mut row_case = |label: String, col: usize| {
        let rook_hot = &rook_rows[..hot];
        let base_hot = &base_rows[..hot];
        let direct = RookColumn::new(&schema, col);
        let only = RowDecoder::new(&schema).only([col]);
        let format = ns_per_row(repeats, reads, || {
            for _ in 0..passes {
                for r in rook_hot {
                    black_box(direct.read(black_box(r)));
                }
            }
        });
        let decoder = ns_per_row(repeats, reads, || {
            for _ in 0..passes {
                for r in rook_hot {
                    only.decode_into(black_box(r), &mut out).unwrap();
                    black_box(&out[col]);
                }
            }
        });
        let base = ns_per_row(repeats, reads, || {
            for _ in 0..passes {
                for r in base_hot {
                    black_box(baseline.read(black_box(r), col));
                }
            }
        });
        println!(
            "{:<34} {:>8.1} {:>8.1} {:>9.1}   {:>9.2}x",
            label,
            format,
            decoder,
            base,
            base / format
        );
    };
    for k in 0..=VARLEN_COLS {
        row_case(
            format!("BIGINT after {} VARCHARs (col {:>2})", k, fixed_col(k)),
            fixed_col(k),
        );
    }
    for k in [0, VARLEN_COLS - 1] {
        row_case(
            format!("VARCHAR #{} (col {:>2})", k + 1, varlen_col(k)),
            varlen_col(k),
        );
    }

    let mut out = Vec::with_capacity(schema.len());
    let rook = ns_per_row(repeats, reads, || {
        for _ in 0..passes {
            for r in &rook_rows[..hot] {
                full.decode_into(black_box(r), &mut out).unwrap();
                black_box(&out);
            }
        }
    });
    let base = ns_per_row(repeats, reads, || {
        for _ in 0..passes {
            for r in &base_rows[..hot] {
                baseline.decode_into(black_box(r), &mut out);
                black_box(&out);
            }
        }
    });
    println!(
        "{:<34} {:>8} {:>8.1} {:>9.1}   {:>9.2}x",
        "decode all 17 columns",
        "-",
        rook,
        base,
        base / rook
    );

    // The same reads over all `rows` rows, which do not fit in cache.
    let col = fixed_col(VARLEN_COLS);
    let direct = RookColumn::new(&schema, col);
    let format = ns_per_row(repeats, rows, || {
        for r in &rook_rows {
            black_box(direct.read(black_box(r)));
        }
    });
    let base = ns_per_row(repeats, rows, || {
        for r in &base_rows {
            black_box(baseline.read(black_box(r), col));
        }
    });
    println!(
        "\nOut of cache ({} rows, {:.0} MB): last BIGINT  format {:.1}  baseline {:.1}  ({:.2}x)",
        rows,
        rook_size as f64 / 1e6,
        format,
        base,
        base / format
    );
}

// ── Part 2: through the engine ────────────────────────────────────────────────

fn run(session: &mut Session, sql: &str) -> usize {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = session
        .execute(plan)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result.rows.map(|(_, rows)| rows.len()).unwrap_or(0)
}

fn ms(repeats: usize, session: &mut Session, sql: &str) -> f64 {
    run(session, sql); // warm the caches
    (0..repeats)
        .map(|_| {
            let t = Instant::now();
            run(session, sql);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .fold(f64::INFINITY, f64::min)
}

fn part2_engine(rows: usize, repeats: usize) {
    let root = std::env::temp_dir().join(format!("rookdb_tuple_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine).expect("init catalog");
    let mut s = Session::new(load_catalog_in(&engine));

    let mut cols = Vec::new();
    for k in 0..VARLEN_COLS {
        cols.push(format!("f{} BIGINT", k));
        cols.push(format!("v{} VARCHAR(64)", k));
    }
    cols.push(format!("f{} BIGINT", VARLEN_COLS));
    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    run(&mut s, &format!("CREATE TABLE w ({})", cols.join(", ")));

    let data = make_rows(rows);
    let load = Instant::now();
    for chunk in data.chunks(250) {
        let values: Vec<String> = chunk
            .iter()
            .map(|row| {
                let vals: Vec<String> = row
                    .iter()
                    .map(|v| match v {
                        Some(DataValue::Varchar(s)) => format!("'{}'", s),
                        Some(DataValue::BigInt(n)) => n.to_string(),
                        _ => unreachable!(),
                    })
                    .collect();
                format!("({})", vals.join(", "))
            })
            .collect();
        run(
            &mut s,
            &format!("INSERT INTO w VALUES {}", values.join(", ")),
        );
    }
    storage_manager::backend::cache::checkpoint_durable().expect("checkpoint");

    println!(
        "\n== Part 2: SQL through the engine ({} rows, loaded in {} ms) ==",
        rows,
        load.elapsed().as_millis()
    );
    println!(
        "   ms, fastest of {} runs; 'all cols' = set_decode_all_columns(true)\n",
        repeats
    );
    println!(
        "{:<44} {:>10} {:>10} {:>9}",
        "query", "read cols", "all cols", "speedup"
    );

    let last = VARLEN_COLS;
    let queries = [
        format!("SELECT SUM(f{}) FROM w", last),
        format!("SELECT COUNT(*) FROM w WHERE f{} % 10 = 0", last),
        format!("SELECT f0, f{} FROM w WHERE f{} < 1000", last, last),
        format!("SELECT COUNT(*) FROM w WHERE v{} = 'x'", VARLEN_COLS - 1),
        "SELECT * FROM w WHERE f0 < 0".to_string(),
    ];
    for sql in &queries {
        engine.set_decode_all_columns(false);
        let pruned = ms(repeats, &mut s, sql);
        let n_pruned = run(&mut s, sql);
        engine.set_decode_all_columns(true);
        let all = ms(repeats, &mut s, sql);
        let n_all = run(&mut s, sql);
        assert_eq!(n_pruned, n_all, "{}: same result either way", sql);
        println!(
            "{:<44} {:>10.2} {:>10.2} {:>8.2}x",
            sql,
            pruned,
            all,
            all / pruned
        );
    }
    engine.set_decode_all_columns(false);

    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let repeats: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(7);

    part1_format(rows, repeats);
    part2_engine(rows, repeats);
}
