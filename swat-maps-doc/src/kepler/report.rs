//! What an import or an export could not carry across, one named line per
//! item, so a user can read and copy it.

/// The report card of an import or an export.
///
/// Every line names its item (a layer label, a filter id, a field name)
/// and never a cell value. `dropped` holds what is not drawn, not applied
/// or not written at all; `approximated` what is drawn, but differently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub dropped: Vec<String>,
    pub approximated: Vec<String>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.approximated.is_empty()
    }

    /// One line per item, dropped first; copyable as it stands.
    pub fn to_text(&self) -> String {
        self.lines().collect::<Vec<_>>().join("\n")
    }

    /// Every line, dropped first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.dropped
            .iter()
            .chain(&self.approximated)
            .map(String::as_str)
    }

    /// A line for something not carried at all (`not drawn: ...`,
    /// `not applied: ...`, `dropped: ...`). A repeat of a line already
    /// held is not added twice.
    pub(crate) fn drop(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.dropped.contains(&line) {
            self.dropped.push(line);
        }
    }

    /// A line for something carried differently (`approximated: ...`).
    pub(crate) fn approx(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.approximated.contains(&line) {
            self.approximated.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_prints_one_line_per_item_dropped_first() {
        let mut r = Report::default();
        assert!(r.is_empty());
        r.approx("approximated: b");
        r.drop("not drawn: a");
        r.drop("not drawn: a");
        assert_eq!(r.to_text(), "not drawn: a\napproximated: b");
        assert!(!r.is_empty());
    }
}
