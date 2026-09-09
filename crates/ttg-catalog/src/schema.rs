//! Typed schema for definition files. This is the contract contributors write against;
//! `docs/MAPPING_FORMAT.md` is the human-readable form of the same thing.

use indexmap::IndexMap;
use serde::Deserialize;
use ttg_core::Value;

// ---------------------------------------------------------------------------
// Resource definitions  (definitions/resources/<type>.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDef {
    pub schema_version: u32,
    pub resource: ResourceMeta,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    #[serde(default)]
    pub relations: Vec<RelationDef>,
    #[serde(default)]
    pub providers: IndexMap<String, ProviderMapping>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceMeta {
    /// Abstract type id, e.g. `subnet`. Must match the file name.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Palette category and default `.tf` file name.
    pub category: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: ResourceKind,
    /// Abstract container types this resource may be placed inside.
    #[serde(default)]
    pub allowed_parents: Vec<String>,
    /// Short glyph shown on the canvas node.
    #[serde(default)]
    pub icon: String,
    /// Warn when nothing links to an instance of this type (e.g. a log group).
    #[serde(default)]
    pub expects_incoming: bool,
    /// A managed service with no network presence: drawing it inside a Virtual Network
    /// is allowed but has no effect, so the app warns.
    #[serde(default)]
    pub network_agnostic: bool,
    /// Providers this type exists on (empty = every provider). A provider-scoped type
    /// is provider-specific by design (Azure Storage Queue); using it with another
    /// target provider is an error rather than a "no mapping" warning.
    #[serde(default)]
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    #[default]
    Node,
    Container,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<toml::Value>,
    /// For `enum` fields.
    #[serde(default)]
    pub options: Vec<String>,
    /// Optional regex the (string) value must match.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Human explanation of `pattern`.
    #[serde(default)]
    pub pattern_hint: Option<String>,
    /// Sub-fields of a `struct_list` row (schema_version 2).
    #[serde(default)]
    pub items: Vec<FieldDef>,
    /// For `entity_ref` fields: abstract types the referenced entity may have (v2).
    #[serde(default)]
    pub targets: Vec<String>,
    /// Required only when the entity has no target for this relation (v2).
    #[serde(default)]
    pub required_unless_relation: Option<String>,
    /// Values must be unique across all entities sharing this scope, per provider (v2).
    #[serde(default)]
    pub unique_scope: Option<String>,
}

impl FieldDef {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
    pub fn default_value(&self) -> Option<Value> {
        self.default.as_ref().and_then(toml_to_value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Bool,
    Int,
    Cidr,
    Enum,
    StringList,
    /// A table of rows, each with the sub-fields declared in `items` (schema_version 2).
    StructList,
    /// The id of another entity on the canvas, limited to `targets` types (v2).
    EntityRef,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationDef {
    /// One of the `ttg_core::Relation` keys.
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Abstract types that may be the target.
    pub targets: Vec<String>,
    #[serde(default)]
    pub cardinality: Cardinality,
    /// If true, being placed inside a container of a target type satisfies this relation.
    #[serde(default)]
    pub via_parent: bool,
    /// Warn when fewer targets than this are linked (e.g. RDS wants two subnets) (v2).
    #[serde(default)]
    pub min_targets: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    /// Exactly one target required.
    One,
    /// Zero or one.
    #[default]
    Optional,
    /// Zero or more.
    Many,
}

// ---------------------------------------------------------------------------
// Per-provider mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMapping {
    #[serde(default)]
    pub status: MappingStatus,
    /// `.tf` file this lands in (without extension). Defaults to the category.
    #[serde(default)]
    pub file: Option<String>,
    /// Free-text notes shown in the inspector.
    #[serde(default)]
    pub notes: String,
    /// Provider-specific extra fields (stored under `provider_config.<provider>`).
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    /// Input variables this mapping needs (emitted into variables.tf when used).
    #[serde(default)]
    pub variables: Vec<VariableDef>,
    #[serde(default)]
    pub blocks: Vec<BlockDef>,
    /// Data-source blocks (`data "<resource>" "<slug>_<key>"`), referenced with
    /// `{ self_data = "<key>", attr = "..." }` (schema_version 2).
    #[serde(default)]
    pub data: Vec<BlockDef>,
    /// Outputs to emit: output name suffix -> what to expose.
    #[serde(default)]
    pub outputs: IndexMap<String, OutputRef>,
    /// Steps the operator must perform by hand after apply.
    #[serde(default)]
    pub manual_steps: Vec<ManualStep>,
    /// Design-time checks evaluated against each entity (v2).
    #[serde(default)]
    pub checks: Vec<CheckDef>,
}

/// A definition-level diagnostic: when the condition holds the message is reported.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDef {
    pub when: Condition,
    /// Evaluate once per row of this field; `{item.<name>}` placeholders then work.
    #[serde(default)]
    pub for_each_field: Option<String>,
    /// `warning` (default) or `error`.
    #[serde(default = "default_severity")]
    pub severity: String,
    /// May use `{name}` and `{item.<name>}` placeholders.
    pub message: String,
}

fn default_severity() -> String {
    "warning".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MappingStatus {
    /// Fully expressed in HCL.
    #[default]
    Full,
    /// Deploys, but `manual_steps` are required to finish.
    Partial,
    /// No resource emitted on purpose (e.g. Resource Group on AWS). Not a warning.
    Logical,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableDef {
    pub name: String,
    #[serde(rename = "type", default = "default_var_type")]
    pub var_type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<toml::Value>,
    #[serde(default)]
    pub sensitive: bool,
}

fn default_var_type() -> String {
    "string".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDef {
    /// Local key, referenced by `self_block`. The first block is the primary one.
    pub key: String,
    /// Concrete resource type, e.g. `aws_subnet`.
    pub resource: String,
    #[serde(default)]
    pub when: Option<Condition>,
    /// Emit one block per row of this `struct_list` / `string_list` field
    /// (schema_version 2). Sources inside may use `{ item = "..." }`.
    #[serde(default)]
    pub for_each_field: Option<String>,
    /// Emit one block per target of this relation (schema_version 2). Sources inside may
    /// use `{ target = "<attr>" }`; `for_each_target_type` narrows the targets.
    #[serde(default)]
    pub for_each_relation: Option<String>,
    #[serde(default)]
    pub for_each_target_type: Option<String>,
    #[serde(default)]
    pub args: IndexMap<String, ArgSource>,
    #[serde(default)]
    pub nested: Vec<NestedBlockDef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NestedBlockDef {
    pub block: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub when: Option<Condition>,
    /// Emit one nested block per row of this field (schema_version 2).
    #[serde(default)]
    pub for_each_field: Option<String>,
    /// Emit one nested block per target of this relation (schema_version 2).
    #[serde(default)]
    pub for_each_relation: Option<String>,
    #[serde(default)]
    pub for_each_target_type: Option<String>,
    #[serde(default)]
    pub args: IndexMap<String, ArgSource>,
    #[serde(default)]
    pub nested: Vec<NestedBlockDef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRef {
    #[serde(default)]
    pub block: Option<String>,
    pub attr: String,
    #[serde(default)]
    pub description: String,
    /// Mark the output `sensitive = true` (required when the attribute is sensitive).
    #[serde(default)]
    pub sensitive: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualStep {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Only applies when this holds (v2). Unconditional when omitted.
    #[serde(default)]
    pub when: Option<Condition>,
}

/// Condition controlling whether a block / nested block is emitted.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// `{ relation = "iam_binding" }` — at least one such edge exists (or via_parent).
    Relation(CondRelation),
    /// `{ field = "versioning" }` — truthy; `{ field = "x", equals = "y" }` — equality.
    Field(CondField),
    /// `{ provider_field = "x" }` / with `equals`.
    ProviderField(CondProviderField),
    /// `{ item = "direction", equals = "ingress" }` — inside a `for_each_field` block
    /// (schema_version 2). `equals_item` compares two sub-fields of the same row.
    Item(CondItem),
    /// `{ all = [ ... ] }` — every condition holds (v2).
    All(CondAll),
    /// `{ any = [ ... ] }` — at least one holds (v2).
    Any(CondAny),
    /// `{ ancestor = "servicebus_namespace" }` — the entity sits inside a container of
    /// that type (`absent = true` inverts) (v2).
    Ancestor(CondAncestor),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondAncestor {
    pub ancestor: String,
    #[serde(default)]
    pub absent: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondAll {
    pub all: Vec<Condition>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondAny {
    pub any: Vec<Condition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondRelation {
    pub relation: String,
    /// Only count targets of this abstract type (schema_version 2).
    #[serde(default)]
    pub target_type: Option<String>,
    /// Invert: holds when there is NO such target (v2).
    #[serde(default)]
    pub absent: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondItem {
    pub item: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    #[serde(default)]
    pub equals_item: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondField {
    pub field: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    /// Holds when the entity has no value for the field (defaults do not count) (v2).
    #[serde(default)]
    pub absent: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondProviderField {
    pub provider_field: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    /// Holds when the entity has no value for the field (defaults do not count) (v2).
    #[serde(default)]
    pub absent: bool,
}

/// String transform applied to a resolved string value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transform {
    /// `My Bucket!` -> `my_bucket`
    Slug,
    /// `My Bucket!` -> `my-bucket`
    Kebab,
    /// lowercase only
    Lower,
    /// lowercase letters and digits only: `My Bucket!` -> `mybucket`
    Alnum,
}

impl Transform {
    pub fn apply(self, s: &str) -> String {
        match self {
            Transform::Slug => ttg_core::slugify(s),
            Transform::Kebab => ttg_core::slugify(s).replace('_', "-"),
            Transform::Lower => s.to_lowercase(),
            Transform::Alnum => s
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect(),
        }
    }
}

/// How to wrap a scalar source value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wrap {
    List,
}

/// Where an argument's value comes from. Serialized as small inline TOML tables whose
/// key set identifies the variant.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgSource {
    /// `{ value = "Standard" }` — literal.
    Literal(SrcLiteral),
    /// `{ field = "cidr_block" }` — abstract field on this entity.
    Field(SrcField),
    /// `{ provider_field = "instance_type" }` — provider-specific field.
    ProviderField(SrcProviderField),
    /// `{ var = "location" }` — `var.<name>` (provider or mapping variable).
    Var(SrcVar),
    /// `{ template = "{name}-nic" }` — `{field}` / `{provider.field}` / `{settings.x}`.
    Template(SrcTemplate),
    /// `{ map = "size", table = { small = "t3.micro" } }` — lookup table.
    Map(SrcMap),
    /// `{ relation = "network_membership", attr = "id" }` — traversal to a related entity.
    Relation(SrcRelation),
    /// `{ ancestor = "resource_group", attr = "name" }` — traversal to an enclosing container.
    Ancestor(SrcAncestor),
    /// `{ self_block = "nic", attr = "id" }` — traversal to another block of this entity.
    SelfBlock(SrcSelfBlock),
    /// `{ object = { Name = { field = "name" } } }`
    Object(SrcObject),
    /// `{ list = [ ... ] }`
    List(SrcList),
    /// `{ func = "jsonencode", args = [ ... ] }` — function call.
    Func(SrcFunc),
    /// `{ item = "port" }` — sub-field of the current `for_each_field` row (v2).
    Item(SrcItem),
    /// `{ item_index = { base = 100, step = 10 } }` — row position as a number (v2).
    ItemIndex(SrcItemIndex),
    /// `{ self_data = "ubuntu", attr = "id" }` — traversal to a data block of this entity (v2).
    SelfData(SrcSelfData),
    /// `{ target = "id" }` — attribute of the current `for_each_relation` target (v2).
    /// `block = "nic"` addresses a secondary block of the target.
    Target(SrcTarget),
    /// `{ item_ref = "source_group", attr = "id" }` — traversal to the entity an
    /// `entity_ref` item of the current row points at (v2).
    ItemRef(SrcItemRef),
    /// `{ entity_var = "value", sensitive = true, description = "..." }` — an input
    /// variable named `<slug>_<name>`, one per entity (v2). For secrets and passwords.
    EntityVar(SrcEntityVar),
    /// `{ if = <condition>, then = <source>, else = <source> }` (v2).
    If(SrcIf),
    /// `{ raw = "..." }` — raw HCL expression. Last resort.
    Raw(SrcRaw),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcLiteral {
    pub value: toml::Value,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcField {
    pub field: String,
    #[serde(default)]
    pub wrap: Option<Wrap>,
    #[serde(default)]
    pub transform: Option<Transform>,
    /// Omit the argument when unset instead of failing.
    #[serde(default)]
    pub optional: bool,
    /// Used when the field is unset (v2).
    #[serde(default)]
    pub fallback: Option<Box<ArgSource>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcProviderField {
    pub provider_field: String,
    #[serde(default)]
    pub wrap: Option<Wrap>,
    #[serde(default)]
    pub transform: Option<Transform>,
    #[serde(default)]
    pub optional: bool,
    /// Used when the field is unset (v2).
    #[serde(default)]
    pub fallback: Option<Box<ArgSource>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcVar {
    pub var: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcTemplate {
    pub template: String,
    #[serde(default)]
    pub transform: Option<Transform>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcMap {
    pub map: String,
    pub table: IndexMap<String, toml::Value>,
    #[serde(default)]
    pub optional: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcRelation {
    pub relation: String,
    pub attr: String,
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub wrap: Option<Wrap>,
    #[serde(default)]
    pub optional: bool,
    /// Only reference targets of this abstract type (v2).
    #[serde(default)]
    pub target_type: Option<String>,
    /// Reference the target's enclosing container of this type instead of the target
    /// itself; targets without one are skipped (v2).
    #[serde(default)]
    pub ancestor: Option<String>,
    /// Used when the reference resolves to nothing (no target, or its block / ancestor is
    /// not emitted) (v2).
    #[serde(default)]
    pub fallback: Option<Box<ArgSource>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcAncestor {
    pub ancestor: String,
    pub attr: String,
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub optional: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcSelfBlock {
    pub self_block: String,
    pub attr: String,
    #[serde(default)]
    pub wrap: Option<Wrap>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcObject {
    pub object: IndexMap<String, ArgSource>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcList {
    pub list: Vec<ArgSource>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcItem {
    pub item: String,
    #[serde(default)]
    pub wrap: Option<Wrap>,
    #[serde(default)]
    pub transform: Option<Transform>,
    #[serde(default)]
    pub optional: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcItemIndex {
    pub item_index: IndexSpec,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    #[serde(default)]
    pub base: i64,
    #[serde(default = "one")]
    pub step: i64,
}
fn one() -> i64 {
    1
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcSelfData {
    pub self_data: String,
    pub attr: String,
    #[serde(default)]
    pub wrap: Option<Wrap>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcItemRef {
    pub item_ref: String,
    pub attr: String,
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub wrap: Option<Wrap>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcEntityVar {
    pub entity_var: String,
    #[serde(rename = "type", default = "default_var_type")]
    pub var_type: String,
    /// May use `{name}` and other template placeholders.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub default: Option<toml::Value>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcTarget {
    pub target: String,
    #[serde(default)]
    pub block: Option<String>,
    #[serde(default)]
    pub wrap: Option<Wrap>,
    /// Reference the target's enclosing container of this type instead (v2).
    #[serde(default)]
    pub ancestor: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcIf {
    #[serde(rename = "if")]
    pub cond: Condition,
    pub then: Box<ArgSource>,
    #[serde(rename = "else", default)]
    pub otherwise: Option<Box<ArgSource>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcFunc {
    pub func: String,
    #[serde(default)]
    pub args: Vec<ArgSource>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcRaw {
    pub raw: String,
}

// ---------------------------------------------------------------------------
// Provider definitions  (definitions/providers/<id>.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    pub schema_version: u32,
    pub provider: ProviderMeta,
    #[serde(default)]
    pub variables: Vec<VariableDef>,
    #[serde(default)]
    pub provider_block: ProviderBlockDef,
    /// Container type every resource must be inside (Azure: `resource_group`).
    #[serde(default)]
    pub required_ancestor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMeta {
    pub id: String,
    pub display_name: String,
    /// Registry namespace, e.g. `hashicorp`.
    pub source_namespace: String,
    /// Registry name, e.g. `aws` / `azurerm`.
    pub source_name: String,
    pub version_constraint: String,
    /// Local name used for `provider "<name>"` and `required_providers`.
    #[serde(default)]
    pub local_name: Option<String>,
    #[serde(default)]
    pub description: String,
}

impl ProviderMeta {
    pub fn local_name(&self) -> &str {
        self.local_name.as_deref().unwrap_or(&self.source_name)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderBlockDef {
    #[serde(default)]
    pub args: IndexMap<String, ArgSource>,
    #[serde(default)]
    pub nested: Vec<NestedBlockDef>,
}

// ---------------------------------------------------------------------------

/// Convert a TOML literal into an IR value (used for defaults and `value = ...`).
pub fn toml_to_value(v: &toml::Value) -> Option<Value> {
    Some(match v {
        toml::Value::String(s) => Value::Str(s.clone()),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Array(a) if a.iter().all(|x| matches!(x, toml::Value::Table(_))) && !a.is_empty() => {
            Value::Records(
                a.iter()
                    .filter_map(|x| match x {
                        toml::Value::Table(t) => Some(
                            t.iter()
                                .filter_map(|(k, v)| toml_to_value(v).map(|v| (k.clone(), v)))
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect(),
            )
        }
        toml::Value::Array(a) => Value::List(
            a.iter()
                .map(|x| match x {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        _ => return None,
    })
}

impl ProviderMeta {
    /// A compact label for badges: "AWS" for Amazon Web Services, "Azure" for Microsoft Azure.
    pub fn short_name(&self) -> String {
        let n = self.display_name.as_str();
        match n {
            "Amazon Web Services" => "AWS".into(),
            "Microsoft Azure" => "Azure".into(),
            "Google Cloud" | "Google Cloud Platform" => "GCP".into(),
            other => other.to_string(),
        }
    }
}
