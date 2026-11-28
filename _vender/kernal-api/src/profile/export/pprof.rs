//! pprof protobuf export (S15 / #644).
//!
//! Encodes `perftools.profiles.Profile` from the vendored schema. Every string
//! in a pprof lives in one table and is referenced by index, so the encoder is
//! mostly an interner plus dedup of `Function` and `Location` records.

use std::collections::HashMap;
use std::io::Write as _;

use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message as _;

use crate::profile::pprof::{Function, Label, Line, Location, Profile, Sample, ValueType};
use crate::profile::SessionResult;

/// Interns strings into a pprof string table.
///
/// Index 0 is always the empty string: the spec requires it, and a reader that
/// finds something else there mis-resolves every field that uses 0 to mean
/// "unset".
#[derive(Debug, Default)]
struct StringTable {
    strings: Vec<String>,
    index: HashMap<String, i64>,
}

impl StringTable {
    fn new() -> Self {
        let mut table = Self::default();
        table.intern("");
        table
    }

    fn intern(&mut self, value: &str) -> i64 {
        if let Some(existing) = self.index.get(value) {
            return *existing;
        }
        let position = self.strings.len() as i64;
        self.strings.push(value.to_string());
        self.index.insert(value.to_string(), position);
        position
    }
}

/// Encode a session as an uncompressed pprof protobuf.
pub fn to_pprof_bytes(result: &SessionResult) -> Vec<u8> {
    build(result).encode_to_vec()
}

/// Encode a session as a gzipped pprof protobuf.
///
/// Gzipped because that is the `.pb.gz` convention every pprof reader expects;
/// the uncompressed form is available for tests that want to decode without a
/// round trip through the compressor.
pub fn to_pprof_gzip(result: &SessionResult) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&to_pprof_bytes(result))?;
    encoder.finish()
}

/// Build the profile message.
pub fn build(result: &SessionResult) -> Profile {
    let mut strings = StringTable::new();

    // Two value types: a sample count, and the wall time each sample stands
    // for. A viewer can then show either "how often" or "how long", and the
    // second is what an operator actually reasons about.
    let samples_type = ValueType {
        r#type: strings.intern("samples"),
        unit: strings.intern("count"),
    };
    let cpu_type = ValueType {
        r#type: strings.intern("cpu"),
        unit: strings.intern("nanoseconds"),
    };

    let mut functions: Vec<Function> = Vec::new();
    let mut function_ids: HashMap<String, u64> = HashMap::new();
    let mut locations: Vec<Location> = Vec::new();
    let mut location_ids: HashMap<String, u64> = HashMap::new();
    let mut samples: Vec<Sample> = Vec::new();

    let period = result.period_nanos as i64;

    for (stack, count) in result.folded() {
        // pprof wants leaf-first; `folded` is root-first because that is the
        // direction flame graphs and the collapsed format use.
        let mut location_id: Vec<u64> = Vec::with_capacity(stack.len());
        for frame in stack.iter().rev() {
            let function_id = *function_ids.entry(frame.clone()).or_insert_with(|| {
                let id = functions.len() as u64 + 1;
                let name = strings.intern(frame);
                functions.push(Function {
                    id,
                    name,
                    system_name: name,
                    filename: 0,
                    start_line: 0,
                });
                id
            });
            let id = *location_ids.entry(frame.clone()).or_insert_with(|| {
                let id = locations.len() as u64 + 1;
                locations.push(Location {
                    id,
                    mapping_id: 0,
                    address: 0,
                    line: vec![Line {
                        function_id,
                        line: 0,
                    }],
                    is_folded: false,
                });
                id
            });
            location_id.push(id);
        }

        samples.push(Sample {
            location_id,
            value: vec![count as i64, count as i64 * period],
            label: Vec::new(),
        });
    }

    Profile {
        sample_type: vec![samples_type, cpu_type],
        sample: samples,
        mapping: Vec::new(),
        location: locations,
        function: functions,
        string_table: strings.strings,
        drop_frames: 0,
        keep_frames: 0,
        time_nanos: result.start_unix_nanos,
        duration_nanos: result.metrics.duration_nanos as i64,
        period_type: Some(cpu_type),
        period,
        comment: Vec::new(),
        default_sample_type: 0,
    }
}

/// Builds an off-CPU / async pprof: five value types, plus task labels.
///
/// Five, because "why is this slow" has several answers and a viewer
/// should be able to weight the same graph by whichever is being asked.
#[derive(Debug, Default)]
pub struct AsyncProfileBuilder {
    strings: StringTable,
    functions: Vec<Function>,
    function_ids: HashMap<String, u64>,
    locations: Vec<Location>,
    location_ids: HashMap<String, u64>,
    samples: Vec<Sample>,
}

impl AsyncProfileBuilder {
    /// An empty builder with the spec-mandated empty string interned at 0.
    pub fn new() -> Self {
        Self {
            strings: StringTable::new(),
            ..Self::default()
        }
    }

    /// Add one task. `stack` is root-first; `values` is
    /// `[idle_ns, busy_ns, scheduled_ns, polls, wakes]`.
    pub fn add_sample(&mut self, stack: &[String], values: [i64; 5], task_name: &str) {
        // pprof wants leaf-first; a spawn chain is naturally root-first.
        let mut location_id = Vec::with_capacity(stack.len());
        for frame in stack.iter().rev() {
            let function_id = match self.function_ids.get(frame) {
                Some(id) => *id,
                None => {
                    let id = self.functions.len() as u64 + 1;
                    let name = self.strings.intern(frame);
                    self.functions.push(Function {
                        id,
                        name,
                        system_name: name,
                        filename: 0,
                        start_line: 0,
                    });
                    self.function_ids.insert(frame.clone(), id);
                    id
                }
            };
            let id = match self.location_ids.get(frame) {
                Some(id) => *id,
                None => {
                    let id = self.locations.len() as u64 + 1;
                    self.locations.push(Location {
                        id,
                        mapping_id: 0,
                        address: 0,
                        line: vec![Line {
                            function_id,
                            line: 0,
                        }],
                        is_folded: false,
                    });
                    self.location_ids.insert(frame.clone(), id);
                    id
                }
            };
            location_id.push(id);
        }

        // The task name rides as a label rather than a frame: it identifies
        // an instance, and folding it into the stack would give every task
        // its own column and defeat the grouping the graph exists for.
        let label = if task_name.is_empty() {
            Vec::new()
        } else {
            vec![Label {
                key: self.strings.intern("task"),
                str: self.strings.intern(task_name),
                num: 0,
                num_unit: 0,
            }]
        };

        self.samples.push(Sample {
            location_id,
            value: values.to_vec(),
            label,
        });
    }

    /// Finish, returning the encoded protobuf.
    pub fn finish(mut self) -> Vec<u8> {
        let nanoseconds = self.strings.intern("nanoseconds");
        let count = self.strings.intern("count");
        let sample_type = vec![
            ValueType {
                r#type: self.strings.intern("idle"),
                unit: nanoseconds,
            },
            ValueType {
                r#type: self.strings.intern("busy"),
                unit: nanoseconds,
            },
            ValueType {
                r#type: self.strings.intern("scheduled"),
                unit: nanoseconds,
            },
            ValueType {
                r#type: self.strings.intern("polls"),
                unit: count,
            },
            ValueType {
                r#type: self.strings.intern("wakes"),
                unit: count,
            },
        ];
        // Index 0 == idle: someone reaching for an off-CPU profile is asking
        // what is *waiting*, and opening on busy time would show them the CPU
        // profile they already had.
        let default_sample_type = sample_type[0].r#type;

        Profile {
            sample_type,
            sample: self.samples,
            mapping: Vec::new(),
            location: self.locations,
            function: self.functions,
            string_table: self.strings.strings,
            drop_frames: 0,
            keep_frames: 0,
            time_nanos: 0,
            duration_nanos: 0,
            period_type: None,
            period: 0,
            comment: Vec::new(),
            default_sample_type,
        }
        .encode_to_vec()
    }
}
