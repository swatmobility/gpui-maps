//! A reference hook in the shape the aggregate-only consumer needs (T8):
//! suppress groups under a threshold, refuse a forbidden column, never
//! re-aggregate. Hosts write their own; this one serves the tests and the
//! canary.

use super::{Cell, QueryShape, RawTable, Refusal, ResultHook};

/// Suppresses every measure cell of a row whose count column is under
/// `min`, and refuses any result that carries a forbidden column.
#[derive(Debug, Clone)]
pub struct MinGroupHook {
    /// The column holding the group size.
    pub count_column: String,
    /// The columns to suppress in a small group (the measures).
    pub measure_columns: Vec<String>,
    pub min: u64,
    pub label_en: String,
    pub label_ja: String,
}

impl MinGroupHook {
    pub fn new(count_column: &str, measures: &[&str], min: u64) -> Self {
        MinGroupHook {
            count_column: count_column.to_string(),
            measure_columns: measures.iter().map(|m| m.to_string()).collect(),
            min,
            label_en: format!("under {min}"),
            label_ja: format!("{min}未満"),
        }
    }
}

impl ResultHook for MinGroupHook {
    fn check(&self, shape: &QueryShape) -> Result<(), Refusal> {
        match shape.first_forbidden() {
            Some(column) => Err(Refusal::ForbiddenColumn { column }),
            None => Ok(()),
        }
    }

    fn apply(&self, shape: &QueryShape, mut raw: RawTable) -> Result<RawTable, Refusal> {
        if let Some(column) = shape.first_forbidden() {
            return Err(Refusal::ForbiddenColumn { column });
        }
        let Some(ci) = raw.column_index(&self.count_column) else { return Ok(raw) };
        let measures: Vec<usize> = std::iter::once(ci)
            .chain(self.measure_columns.iter().filter_map(|m| raw.column_index(m)))
            .collect();
        for r in 0..raw.row_count() {
            let small = match raw.cell(r, ci) {
                Cell::Value(v) => v.as_f64().is_some_and(|n| n < self.min as f64),
                _ => false,
            };
            if small {
                for &c in &measures {
                    raw.set_suppressed(r, c);
                }
            }
        }
        Ok(raw)
    }

    fn suppressed_label(&self, locale: &str) -> String {
        if locale.starts_with("ja") {
            self.label_ja.clone()
        } else {
            self.label_en.clone()
        }
    }
}
