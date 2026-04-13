use indexmap::IndexMap;

/// The maximum depth sentinel — when depth budget is exhausted we emit Any.
pub const DEPTH_EXHAUSTED: &str = "<depth limit>";

// ---------------------------------------------------------------------------
// Scalar primitive types (the only things that can participate in a Union)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalarType {
    Boolean,
    Integer,
    Float,  // LUB(Integer, Float) = Float; stored as Float in a union
    Str,
}

impl ScalarType {
    pub fn json_schema_type(&self) -> &'static str {
        match self {
            ScalarType::Boolean => "boolean",
            ScalarType::Integer => "integer",
            ScalarType::Float   => "number",
            ScalarType::Str     => "string",
        }
    }
}

// ---------------------------------------------------------------------------
// Per-field metadata inside an Object schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub schema: InferredSchema,
    /// How many records contained this field (used to determine required).
    /// "required" is intentionally NOT emitted in JSON Schema output per spec,
    /// but kept here for potential future use / stats.
    pub occurrences: u64,
}

// ---------------------------------------------------------------------------
// Core schema type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum InferredSchema {
    /// No observations yet — identity element for LUB.
    Never,

    /// Accepts anything: union cap exceeded, depth exceeded, or empty file.
    Any,

    /// A single scalar type, possibly nullable.
    Scalar {
        ty: ScalarType,
        nullable: bool,
    },

    /// Null seen in isolation (before being merged with anything else).
    /// Treated as nullable Never — LUB will absorb it immediately.
    NullOnly,

    /// A homogeneous (unified) array.
    Array {
        items: Box<InferredSchema>,
        nullable: bool,
    },

    /// An object with named fields.
    Object {
        /// Preserves insertion order of first-seen fields.
        fields: IndexMap<String, FieldInfo>,
        nullable: bool,
    },

    /// A union of scalars only (never Object or Array).
    /// Object+Scalar or Array+Scalar → Any immediately.
    Union {
        variants: std::collections::BTreeSet<ScalarType>,
        nullable: bool,
    },
}

impl InferredSchema {
    /// Returns true if this schema permits null values.
    pub fn is_nullable(&self) -> bool {
        match self {
            InferredSchema::Never    => false,
            InferredSchema::Any      => true,
            InferredSchema::NullOnly => true,
            InferredSchema::Scalar   { nullable, .. } => *nullable,
            InferredSchema::Array    { nullable, .. } => *nullable,
            InferredSchema::Object   { nullable, .. } => *nullable,
            InferredSchema::Union    { nullable, .. } => *nullable,
        }
    }

    /// Promote this schema to nullable (returns a clone with nullable=true).
    pub fn into_nullable(self) -> Self {
        match self {
            InferredSchema::Never    => InferredSchema::NullOnly,
            InferredSchema::NullOnly => InferredSchema::NullOnly,
            InferredSchema::Any      => InferredSchema::Any,
            InferredSchema::Scalar   { ty, .. }               => InferredSchema::Scalar   { ty,      nullable: true },
            InferredSchema::Array    { items, .. }            => InferredSchema::Array    { items,   nullable: true },
            InferredSchema::Object   { fields, .. }           => InferredSchema::Object   { fields,  nullable: true },
            InferredSchema::Union    { variants, .. }         => InferredSchema::Union    { variants, nullable: true },
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level accumulator
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Accumulator {
    pub root: InferredSchema,
    pub record_count: u64,
    pub warning_count: u64,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self {
            root: InferredSchema::Never,
            record_count: 0,
            warning_count: 0,
        }
    }
}
