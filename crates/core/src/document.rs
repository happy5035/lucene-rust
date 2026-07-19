/// A single field value in a [`Document`].
///
/// Index behavior is declared by the schema (`FieldSpec`), not by the value:
/// the same `Keyword` value may be indexed, doc-valued, stored, or any
/// combination. The value variant must match the field's declared kind
/// (see `FieldSpec` constructors).
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// Text to be tokenized (whitespace) and/or stored.
    Text(String),
    /// Whole-string term (StringField semantics) and/or SortedDocValues value.
    Keyword(String),
    /// LongPoint and/or NumericDocValues and/or stored long.
    Long(i64),
    /// IntPoint and/or NumericDocValues and/or stored int.
    Int(i32),
}

/// A document: an unordered bag of (field name, value) pairs.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub fields: Vec<(String, FieldValue)>,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, value: FieldValue) -> &mut Self {
        self.fields.push((name.to_string(), value));
        self
    }
}
