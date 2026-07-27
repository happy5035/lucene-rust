use rustlucene_core::schema::{FieldSpec, Schema};

/// 构造 V5 metric Schema。见 V5 Format Reference R3。
pub fn v5_schema() -> Schema {
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("metric_name").with_sorted_dv());
    s.add(FieldSpec::text("metric_labels"));
    s.add(FieldSpec::long_point("series_hash").with_numeric_dv());
    s.add(FieldSpec::long_point("time_min").with_numeric_dv());
    s.add(FieldSpec::long_point("time_max").with_numeric_dv());
    s.add(FieldSpec::numeric_dv("sample_count"));
    s.add(FieldSpec::binary_dv("gorilla_data"));
    s
}

/// Downsample shard schema (5m/1h). Same as V5 but:
/// - `bucket_count` replaces `sample_count`
/// - `downsample_data` replaces `gorilla_data`
pub fn downsample_schema() -> Schema {
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("metric_name").with_sorted_dv());
    s.add(FieldSpec::text("metric_labels"));
    s.add(FieldSpec::long_point("series_hash").with_numeric_dv());
    s.add(FieldSpec::long_point("time_min").with_numeric_dv());
    s.add(FieldSpec::long_point("time_max").with_numeric_dv());
    s.add(FieldSpec::numeric_dv("bucket_count"));
    s.add(FieldSpec::binary_dv("downsample_data"));
    s
}
