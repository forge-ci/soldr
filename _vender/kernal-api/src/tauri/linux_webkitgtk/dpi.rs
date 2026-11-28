//! GTK-thread-owned correction of desktop DPI without cumulative scaling.
#[derive(Default)]
pub(crate) struct Correction {
    original: Option<i32>,
    applied: Option<i32>,
}

impl Correction {
    pub(crate) fn update(&mut self, current: i32, scale: i32) -> i32 {
        if self.applied != Some(current) {
            self.original = Some(current);
        }
        let original = self.original.unwrap_or(current);
        let applied = if original <= 0 {
            96 * 1024
        } else {
            (original / scale.max(1)).max(1)
        };
        self.applied = Some(applied);
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::Correction;

    #[test]
    fn repeated_views_and_scale_changes_use_original_desktop_dpi() {
        let mut correction = Correction::default();
        assert_eq!(correction.update(172_032, 2), 86_016);
        assert_eq!(correction.update(86_016, 2), 86_016);
        assert_eq!(correction.update(86_016, 1), 172_032);
        assert_eq!(correction.update(172_032, 2), 86_016);
    }

    #[test]
    fn observed_desktop_changes_replace_original_dpi() {
        let mut correction = Correction::default();
        assert_eq!(correction.update(172_032, 2), 86_016);
        assert_eq!(correction.update(196_608, 2), 98_304);
        assert_eq!(correction.update(98_304, 2), 98_304);
    }

    #[test]
    fn unknown_dpi_keeps_fallback_across_views() {
        let mut correction = Correction::default();
        assert_eq!(correction.update(-1, 2), 98_304);
        assert_eq!(correction.update(98_304, 2), 98_304);
        assert_eq!(correction.update(0, 1), 98_304);
    }
}
