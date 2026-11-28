//! Firefox Profiler JSON export (S15 / #644).
//!
//! Emits the "processed profile" shape the Firefox Profiler UI ingests
//! directly, so an operator can drag an export onto profiler.firefox.com and
//! get its call tree, stack chart, and inverted view for free.
//!
//! The format is column-oriented: a thread carries parallel arrays
//! (`stackTable`, `frameTable`, `funcTable`) that reference each other by
//! index, with every string in one `stringArray`. That is what makes it
//! compact for large profiles, and it is why the encoder below is written as a
//! set of interning tables rather than as nested objects.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::profile::SessionResult;

/// Interning tables for one thread.
#[derive(Debug, Default)]
struct Tables {
    strings: Vec<String>,
    string_index: HashMap<String, usize>,

    func_names: Vec<usize>,
    func_index: HashMap<usize, usize>,

    frame_funcs: Vec<usize>,
    frame_index: HashMap<usize, usize>,

    /// `stackTable` rows: `(prefix, frame)`. A stack is a linked list through
    /// `prefix`, which is what lets two stacks that share a root share its
    /// rows instead of repeating them.
    stack_prefix: Vec<Option<usize>>,
    stack_frame: Vec<usize>,
    stack_index: HashMap<(Option<usize>, usize), usize>,
}

impl Tables {
    fn string(&mut self, value: &str) -> usize {
        if let Some(existing) = self.string_index.get(value) {
            return *existing;
        }
        let position = self.strings.len();
        self.strings.push(value.to_string());
        self.string_index.insert(value.to_string(), position);
        position
    }

    fn func(&mut self, name: &str) -> usize {
        let name_index = self.string(name);
        if let Some(existing) = self.func_index.get(&name_index) {
            return *existing;
        }
        let position = self.func_names.len();
        self.func_names.push(name_index);
        self.func_index.insert(name_index, position);
        position
    }

    fn frame(&mut self, name: &str) -> usize {
        let func = self.func(name);
        if let Some(existing) = self.frame_index.get(&func) {
            return *existing;
        }
        let position = self.frame_funcs.len();
        self.frame_funcs.push(func);
        self.frame_index.insert(func, position);
        position
    }

    fn stack(&mut self, prefix: Option<usize>, frame: usize) -> usize {
        if let Some(existing) = self.stack_index.get(&(prefix, frame)) {
            return *existing;
        }
        let position = self.stack_prefix.len();
        self.stack_prefix.push(prefix);
        self.stack_frame.push(frame);
        self.stack_index.insert((prefix, frame), position);
        position
    }
}

/// Render a session as Firefox Profiler JSON.
pub fn to_firefox_json(result: &SessionResult) -> String {
    to_firefox_value(result).to_string()
}

/// Render a session as a `serde_json::Value`, so a test can assert on it
/// without reparsing.
pub fn to_firefox_value(result: &SessionResult) -> Value {
    let mut tables = Tables::default();
    let mut sample_stacks: Vec<usize> = Vec::new();
    let mut sample_times: Vec<f64> = Vec::new();
    let mut sample_weights: Vec<u64> = Vec::new();

    let interval_ms = result.period_nanos as f64 / 1_000_000.0;
    let mut clock_ms = 0.0f64;

    for (stack, count) in result.folded() {
        // Root-first, building the prefix chain as we descend.
        let mut prefix: Option<usize> = None;
        for frame_name in &stack {
            let frame = tables.frame(frame_name);
            prefix = Some(tables.stack(prefix, frame));
        }
        let Some(leaf) = prefix else {
            continue;
        };
        // One row per folded stack, weighted by its count. Emitting `count`
        // identical rows instead would be equivalent but would inflate a long
        // profile by orders of magnitude, and `weight` exists precisely so it
        // does not have to.
        sample_stacks.push(leaf);
        sample_times.push(clock_ms);
        sample_weights.push(count);
        clock_ms += interval_ms * count as f64;
    }

    let thread = json!({
        "name": "All threads",
        "isMainThread": true,
        "processType": "default",
        "processName": "profiled",
        "pid": "0",
        "tid": 0,
        "registerTime": 0,
        "unregisterTime": Value::Null,
        "processStartupTime": 0,
        "processShutdownTime": Value::Null,
        "samples": {
            "stack": sample_stacks,
            "time": sample_times,
            "weight": sample_weights,
            "weightType": "samples",
            "length": sample_weights.len(),
        },
        "markers": { "data": [], "name": [], "startTime": [], "endTime": [], "phase": [], "category": [], "length": 0 },
        "stackTable": {
            "prefix": tables.stack_prefix.iter().map(|p| match p {
                Some(index) => json!(index),
                None => Value::Null,
            }).collect::<Vec<_>>(),
            "frame": tables.stack_frame,
            "category": vec![0usize; tables.stack_prefix.len()],
            "subcategory": vec![0usize; tables.stack_prefix.len()],
            "length": tables.stack_prefix.len(),
        },
        "frameTable": {
            "func": tables.frame_funcs.clone(),
            "category": vec![Value::Null; tables.frame_funcs.len()],
            "subcategory": vec![Value::Null; tables.frame_funcs.len()],
            "address": vec![-1i64; tables.frame_funcs.len()],
            "line": vec![Value::Null; tables.frame_funcs.len()],
            "column": vec![Value::Null; tables.frame_funcs.len()],
            "innerWindowID": vec![Value::Null; tables.frame_funcs.len()],
            "implementation": vec![Value::Null; tables.frame_funcs.len()],
            "inlineDepth": vec![0usize; tables.frame_funcs.len()],
            "nativeSymbol": vec![Value::Null; tables.frame_funcs.len()],
            "length": tables.frame_funcs.len(),
        },
        "funcTable": {
            "name": tables.func_names.clone(),
            "isJS": vec![false; tables.func_names.len()],
            "relevantForJS": vec![false; tables.func_names.len()],
            "resource": vec![-1i64; tables.func_names.len()],
            "fileName": vec![Value::Null; tables.func_names.len()],
            "lineNumber": vec![Value::Null; tables.func_names.len()],
            "columnNumber": vec![Value::Null; tables.func_names.len()],
            "length": tables.func_names.len(),
        },
        "resourceTable": { "lib": [], "name": [], "host": [], "type": [], "length": 0 },
        "nativeSymbols": { "libIndex": [], "address": [], "name": [], "functionSize": [], "length": 0 },
        "stringArray": tables.strings,
    });

    json!({
        "meta": {
            "version": 27,
            "preprocessedProfileVersion": 48,
            "interval": interval_ms,
            "startTime": result.start_unix_nanos as f64 / 1_000_000.0,
            "processType": 0,
            "product": "rpprobed",
            "stackwalk": 1,
            "sampleUnits": { "time": "ms", "eventDelay": "ms", "threadCPUDelta": "ns" },
            "categories": [
                { "name": "Other", "color": "grey", "subcategories": ["Other"] },
                { "name": "Native", "color": "blue", "subcategories": ["Other"] },
            ],
            "markerSchema": [],
        },
        "libs": [],
        "threads": [thread],
    })
}
