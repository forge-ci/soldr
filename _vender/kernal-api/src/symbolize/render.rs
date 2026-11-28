//! Rendering a symbol report for a human to read (#637).
//!
//! # Why a text form at all, when the JSON is complete
//!
//! The JSON is for tools. A stack is read by a person deciding where to look
//! next, and that decision is made from shape — which thread is stuck, how
//! deep, in whose code. Scanning nested JSON for that is work the reader
//! should not have to do.
//!
//! # Degradation has to be visible
//!
//! A frame that could not be resolved is printed with its module and offset
//! and no name, and one that could not even be attributed says so. The reader
//! must be able to tell "this frame is in code we have no symbols for" from
//! "this frame is in a function called nothing" — which is why an unresolved
//! frame never renders as a blank where a name would go.

use kernal_api::symbolize::wire::{FrameStatus, ModuleSymbolStatus, SymFrame, SymbolReport};

/// Render `report` as a human-readable stack dump.
pub fn render_text(report: &SymbolReport) -> String {
    let mut out = String::new();

    if report.threads.is_empty() {
        // Distinct from "the threads had no frames", which renders as thread
        // headers with empty bodies.
        out.push_str("(no threads in report)\n");
        return out;
    }

    // Modules whose symbols were expected but unusable, listed before the
    // stacks. A reader scanning unnamed frames should not have to infer that
    // the wrong PDB is on disk somewhere — that is a fixable misconfiguration
    // and worth saying outright.
    let problems: Vec<_> = report
        .modules
        .iter()
        .filter(|m| m.status == ModuleSymbolStatus::Mismatched)
        .collect();
    if !problems.is_empty() {
        out.push_str("Symbol problems\n");
        for module in problems {
            out.push_str(&format!(
                "  {}: {} candidate(s) found but none matched this build\n",
                module.name, module.rejected_candidates
            ));
        }
        out.push('\n');
    }

    for (position, thread) in report.threads.iter().enumerate() {
        if position > 0 {
            out.push('\n');
        }
        match &thread.name {
            Some(name) => out.push_str(&format!("Thread {} ({name})\n", thread.os_tid)),
            None => out.push_str(&format!("Thread {}\n", thread.os_tid)),
        }

        if thread.frames.is_empty() && thread.py_frames.is_empty() {
            out.push_str("  (no frames captured)\n");
            continue;
        }

        for (depth, frame) in thread.frames.iter().enumerate() {
            out.push_str(&format!("  #{depth:<3} {}\n", render_frame(frame)));
            // Inlined call sites are indented under the physical frame they
            // were folded into, so the machine stack's depth stays readable.
            for inline in &frame.inline_frames {
                let location = match (&inline.file, inline.line) {
                    (Some(file), Some(line)) => format!("  ({file}:{line})"),
                    (Some(file), None) => format!("  ({file})"),
                    _ => String::new(),
                };
                out.push_str(&format!("       [inlined] {}{location}\n", inline.function));
            }
        }

        if !thread.py_frames.is_empty() {
            // Labelled, because a reader must not mistake interpreter frames
            // for machine frames of the same thread — they describe the same
            // moment at different levels.
            out.push_str("  Python:\n");
            for frame in &thread.py_frames {
                out.push_str(&format!(
                    "    {}:{} in {}\n",
                    frame.file, frame.line, frame.func
                ));
            }
        }
    }
    out
}

fn render_frame(frame: &SymFrame) -> String {
    let location = format!("{}+{:#x}", frame.module, frame.relative_address);
    match frame.status {
        FrameStatus::Resolved => {
            let name = frame.function.as_deref().unwrap_or("<unnamed>");
            match (&frame.file, frame.line) {
                (Some(file), Some(line)) => format!("{location}  {name}  ({file}:{line})"),
                (Some(file), None) => format!("{location}  {name}  ({file})"),
                _ => format!("{location}  {name}"),
            }
        }
        // Explicit markers rather than an empty column: a blank where a name
        // belongs reads as a function with no name, not as absent symbols.
        FrameStatus::RawOnly => format!("{location}  <no symbols for this module>"),
        FrameStatus::ModuleUnknown => format!("{location}  <address matched no module>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernal_api::symbolize::wire::{InlineFrame, PyFrame, SymThread};

    fn frame(module: &str, addr: u64, status: FrameStatus, function: Option<&str>) -> SymFrame {
        SymFrame {
            module: module.into(),
            relative_address: addr,
            function: function.map(str::to_owned),
            file: None,
            line: None,
            inline_frames: Vec::new(),
            status,
        }
    }

    fn report(threads: Vec<SymThread>) -> SymbolReport {
        SymbolReport {
            threads,
            ..Default::default()
        }
    }

    /// A mismatched module is called out, because unnamed frames alone do
    /// not tell a reader that the wrong symbols are on disk.
    #[test]
    fn mismatched_modules_are_reported_above_the_stacks() {
        use kernal_api::symbolize::wire::{ModuleReport, ModuleSymbolStatus};

        let mut r = report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![frame("a.dll", 0x10, FrameStatus::RawOnly, None)],
            py_frames: Vec::new(),
        }]);
        r.modules = vec![ModuleReport {
            name: "a.dll".into(),
            status: ModuleSymbolStatus::Mismatched,
            rejected_candidates: 2,
            ..Default::default()
        }];
        let text = render_text(&r);

        assert!(text.contains("Symbol problems"), "{text}");
        assert!(text.contains("a.dll: 2 candidate"), "{text}");
        let problems_at = text.find("Symbol problems").unwrap();
        let thread_at = text.find("Thread 1").unwrap();
        assert!(problems_at < thread_at, "problems must precede the stacks");
    }

    /// A report with nothing wrong must not grow a noisy empty section.
    #[test]
    fn a_clean_report_has_no_problems_section() {
        let r = report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![frame("a.dll", 0x10, FrameStatus::Resolved, Some("f"))],
            py_frames: Vec::new(),
        }]);
        assert!(!render_text(&r).contains("Symbol problems"));
    }

    #[test]
    fn a_resolved_frame_shows_its_name_module_and_offset() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 42,
            name: None,
            frames: vec![frame(
                "a.dll",
                0x1234,
                FrameStatus::Resolved,
                Some("do_work"),
            )],
            py_frames: Vec::new(),
        }]));

        assert!(text.contains("Thread 42"), "{text}");
        assert!(text.contains("a.dll+0x1234"), "{text}");
        assert!(text.contains("do_work"), "{text}");
    }

    /// The distinction a reader depends on: no symbols vs no module.
    #[test]
    fn the_two_degradations_are_distinguishable() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![
                frame("a.dll", 0x10, FrameStatus::RawOnly, None),
                frame("<unknown>", 0x20, FrameStatus::ModuleUnknown, None),
            ],
            py_frames: Vec::new(),
        }]));

        assert!(text.contains("<no symbols for this module>"), "{text}");
        assert!(text.contains("<address matched no module>"), "{text}");
    }

    /// An unresolved frame must never render as a blank where a name goes —
    /// that reads as a function called nothing.
    #[test]
    fn an_unresolved_frame_never_renders_an_empty_name() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![frame("a.dll", 0x10, FrameStatus::RawOnly, None)],
            py_frames: Vec::new(),
        }]));

        for line in text.lines().filter(|l| l.contains("a.dll")) {
            assert!(
                !line.trim_end().ends_with("a.dll+0x10"),
                "the frame line stops after the offset, leaving the name column \
                 blank: {line:?}"
            );
        }
    }

    #[test]
    fn offsets_are_preserved_even_when_unresolved() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![frame("a.dll", 0xDEAD, FrameStatus::RawOnly, None)],
            py_frames: Vec::new(),
        }]));
        assert!(
            text.contains("0xdead"),
            "the offset is the residual value: {text}"
        );
    }

    #[test]
    fn python_frames_are_labelled_separately_from_native_ones() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 5,
            name: None,
            frames: vec![frame(
                "a.dll",
                0x10,
                FrameStatus::Resolved,
                Some("native_fn"),
            )],
            py_frames: vec![PyFrame {
                file: "app.py".into(),
                line: 12,
                func: "handler".into(),
            }],
        }]));

        assert!(text.contains("Python:"), "{text}");
        assert!(text.contains("app.py:12 in handler"), "{text}");
        // The label must come after the machine frames, so the two are not
        // read as one stack.
        let python_at = text.find("Python:").unwrap();
        let native_at = text.find("native_fn").unwrap();
        assert!(native_at < python_at, "{text}");
    }

    #[test]
    fn frames_are_numbered_by_depth() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![
                frame("a.dll", 0x10, FrameStatus::Resolved, Some("inner")),
                frame("a.dll", 0x20, FrameStatus::Resolved, Some("outer")),
            ],
            py_frames: Vec::new(),
        }]));
        let inner = text.find("inner").unwrap();
        let outer = text.find("outer").unwrap();
        assert!(inner < outer, "innermost frame must print first: {text}");
        assert!(text.contains("#0"), "{text}");
        assert!(text.contains("#1"), "{text}");
    }

    #[test]
    fn inlined_frames_are_marked_and_nested() {
        let mut f = frame("a.dll", 0x10, FrameStatus::Resolved, Some("outer"));
        f.inline_frames = vec![InlineFrame {
            function: "folded".into(),
            file: Some("src/x.rs".into()),
            line: Some(7),
        }];
        let text = render_text(&report(vec![SymThread {
            os_tid: 1,
            name: None,
            frames: vec![f],
            py_frames: Vec::new(),
        }]));

        assert!(text.contains("[inlined] folded"), "{text}");
        assert!(text.contains("src/x.rs:7"), "{text}");
    }

    #[test]
    fn a_thread_name_is_shown_when_known() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 9,
            name: Some("rp-probe".into()),
            frames: Vec::new(),
            py_frames: Vec::new(),
        }]));
        assert!(text.contains("Thread 9 (rp-probe)"), "{text}");
    }

    /// An empty report must say so rather than rendering nothing, which would
    /// be indistinguishable from the renderer failing.
    #[test]
    fn an_empty_report_says_so() {
        let text = render_text(&report(Vec::new()));
        assert!(!text.trim().is_empty());
        assert!(text.contains("no threads"), "{text}");
    }

    #[test]
    fn a_thread_with_no_frames_is_reported_as_such() {
        let text = render_text(&report(vec![SymThread {
            os_tid: 3,
            name: None,
            frames: Vec::new(),
            py_frames: Vec::new(),
        }]));
        assert!(text.contains("Thread 3"), "{text}");
        assert!(text.contains("no frames captured"), "{text}");
    }
}
