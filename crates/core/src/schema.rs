use codec_lucene9::{DocValuesType, IndexOptions};

/// Point (BKD) indexing parameters for a field. Only 1D numeric points are
/// supported (LongPoint/IntPoint semantics: numDims == numIndexDims == 1,
/// document/FieldType.java:292-295).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointSpec {
    /// 8 = long (LongPoint), 4 = int (IntPoint).
    pub bytes_per_dim: u8,
}

/// Per-field write behavior, declared once in the [`Schema`].
///
/// Mirrors the Lucene `FieldType` subset this project supports: the no-scoring
/// profile — all indexed text fields use `omit_norms = true`, positions are a
/// per-field opt-in (for phrase queries on message-like fields). A field can
/// combine capabilities like Lucene fields sharing a name (e.g. a timestamp
/// field that is LongPoint + NumericDocValues + stored at once).
#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub name: String,
    /// NONE = not inverted-indexed (no postings).
    pub index_options: IndexOptions,
    /// Must be true for indexed fields (we never write norms).
    pub omit_norms: bool,
    pub stored: bool,
    /// true = tokenized text (whitespace); false = whole value is one term
    /// (StringField semantics). Meaningless when not indexed.
    pub tokenized: bool,
    pub doc_values: DocValuesType,
    pub points: Option<PointSpec>,
    /// Analyzer spec text (e.g. "whitespace|lowercase"), only legal on
    /// indexed tokenized text fields. Not persisted — analyzer config is
    /// application-level, same as Lucene.
    pub analyzer: Option<String>,
}

impl FieldSpec {
    /// Text field: tokenized, DOCS_AND_FREQS, omitNorms, stored. The M1 profile.
    pub fn text(name: &str) -> Self {
        Self {
            name: name.to_string(),
            index_options: IndexOptions::DocsAndFreqs,
            omit_norms: true,
            stored: true,
            tokenized: true,
            doc_values: DocValuesType::None,
            points: None,
            analyzer: None,
        }
    }

    /// Text field with positions enabled (phrase-query capable message field).
    pub fn text_with_positions(name: &str) -> Self {
        Self {
            index_options: IndexOptions::DocsAndFreqsAndPositions,
            ..Self::text(name)
        }
    }

    /// Keyword field (StringField semantics): whole value is one term, DOCS,
    /// omitNorms, stored.
    pub fn keyword(name: &str) -> Self {
        Self {
            name: name.to_string(),
            index_options: IndexOptions::Docs,
            omit_norms: true,
            stored: true,
            tokenized: false,
            doc_values: DocValuesType::None,
            points: None,
            analyzer: None,
        }
    }

    /// 1D long point (LongPoint). Not stored, no doc values by default —
    /// chain [`Self::with_numeric_dv`] / [`Self::with_stored`] for the log
    /// timestamp profile (range query + sort + retrieval).
    pub fn long_point(name: &str) -> Self {
        Self {
            points: Some(PointSpec { bytes_per_dim: 8 }),
            ..Self::base(name)
        }
    }

    /// 1D int point (IntPoint).
    pub fn int_point(name: &str) -> Self {
        Self {
            points: Some(PointSpec { bytes_per_dim: 4 }),
            ..Self::base(name)
        }
    }

    /// NumericDocValues-only field (i64 values, accepts Int too).
    pub fn numeric_dv(name: &str) -> Self {
        Self {
            doc_values: DocValuesType::Numeric,
            ..Self::base(name)
        }
    }

    /// SortedDocValues-only field (Keyword values).
    pub fn sorted_dv(name: &str) -> Self {
        Self {
            doc_values: DocValuesType::Sorted,
            ..Self::base(name)
        }
    }

    /// BinaryDocValues-only field (arbitrary bytes).
    pub fn binary_dv(name: &str) -> Self {
        Self {
            doc_values: DocValuesType::Binary,
            ..Self::base(name)
        }
    }

    /// Stored-only field.
    pub fn stored(name: &str) -> Self {
        Self {
            stored: true,
            ..Self::base(name)
        }
    }

    fn base(name: &str) -> Self {
        Self {
            name: name.to_string(),
            index_options: IndexOptions::None,
            omit_norms: false,
            stored: false,
            tokenized: false,
            doc_values: DocValuesType::None,
            points: None,
            analyzer: None,
        }
    }

    /// Adds NumericDocValues to this field (e.g. a LongPoint timestamp that
    /// also needs sort-by-value).
    pub fn with_numeric_dv(mut self) -> Self {
        self.doc_values = DocValuesType::Numeric;
        self
    }

    /// Adds SortedDocValues to this field (accepts Keyword values).
    pub fn with_sorted_dv(mut self) -> Self {
        self.doc_values = DocValuesType::Sorted;
        self
    }

    /// Adds BinaryDocValues to this field (e.g. a TextField whose raw bytes
    /// must be readable at merge/downsample time — Lucene same-name pattern).
    pub fn with_binary_dv(mut self) -> Self {
        self.doc_values = DocValuesType::Binary;
        self
    }

    /// Marks this field stored (or not).
    pub fn with_stored(mut self, stored: bool) -> Self {
        self.stored = stored;
        self
    }

    /// Attaches an analyzer chain spec ("tokenizer|filter|...") to this
    /// field. Only valid on indexed tokenized text fields (enforced by
    /// `Schema::add`).
    pub fn with_analyzer(mut self, spec: &str) -> Self {
        self.analyzer = Some(spec.to_string());
        self
    }

    pub fn is_indexed(&self) -> bool {
        self.index_options != IndexOptions::None
    }

    pub fn has_positions(&self) -> bool {
        matches!(
            self.index_options,
            IndexOptions::DocsAndFreqsAndPositions
                | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
        )
    }
}

/// The set of fields an [`crate::IndexWriter`] accepts.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    fields: Vec<FieldSpec>,
}

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, spec: FieldSpec) -> &mut Self {
        assert!(
            !self.fields.iter().any(|f| f.name == spec.name),
            "duplicate field {}",
            spec.name
        );
        if spec.is_indexed() {
            assert!(
                spec.omit_norms,
                "field {}: norms are never written",
                spec.name
            );
            assert!(
                spec.tokenized || !spec.has_positions(),
                "field {}: keyword fields cannot carry positions",
                spec.name
            );
        }
        if let Some(a) = &spec.analyzer {
            assert!(
                spec.tokenized && spec.is_indexed(),
                "field {}: analyzer requires an indexed tokenized text field",
                spec.name
            );
            if let Err(e) = crate::analysis::Analyzer::parse(a) {
                panic!("field {}: invalid analyzer spec: {e}", spec.name);
            }
        }
        if let Some(p) = spec.points {
            assert!(
                p.bytes_per_dim == 4 || p.bytes_per_dim == 8,
                "field {}: only 1D int/long points are supported",
                spec.name
            );
        }
        self.fields.push(spec);
        self
    }

    pub fn get(&self, name: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_only_on_indexed_tokenized_fields() {
        let result = {
            let mut s = Schema::new();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.add(FieldSpec::keyword("level").with_analyzer("whitespace"));
            }))
        };
        assert!(result.is_err(), "keyword field with analyzer must be rejected");
    }

    #[test]
    fn with_analyzer_roundtrip() {
        let f = FieldSpec::text("message").with_analyzer("whitespace|lowercase");
        assert_eq!(f.analyzer.as_deref(), Some("whitespace|lowercase"));
    }
}
