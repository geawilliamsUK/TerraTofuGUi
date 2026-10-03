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
    /// First word of the environment variables a Kubernetes workload linked to this type
    /// receives: `QUEUE` gives `QUEUE_<NAME>_<KEY>` for each `connection` key (v2).
    /// Defaults to the type id in upper case.
    #[serde(default)]
    pub env_prefix: Option<String>,
}

impl ResourceMeta {
    /// `env_prefix`, or the type id in upper case.
    pub fn env_prefix(&self) -> String {
        self.env_prefix
            .clone()
            .unwrap_or_else(|| self.type_id.to_uppercase())
    }
}

/// Abstract types the Kubernetes manifests export reads its own fields and links from.
/// Only these may mark a field or relation `manifests = true`.
pub const MANIFEST_TYPES: &[&str] = &["kubernetes_workload"];

/// Project settings a `{ setting = "…" }` condition may test.
pub const CONDITION_SETTINGS: &[&str] = &["kubernetes_manifests"];

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
    /// For `enum` fields, the only values allowed. For `string_list` fields, an optional
    /// restriction on each entry (empty means any string, as before) (v2).
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
    /// Read by the Kubernetes manifests export rather than by a provider mapping (v2):
    /// the field counts as used by every provider (see `MANIFEST_TYPES`).
    #[serde(default)]
    pub manifests: bool,
    /// A `bool` field whose `true` puts a secret value into the state file (a generated
    /// password). The diagnostics warn while such a value meets local or unencrypted
    /// state (see `ttg-codegen::state`).
    #[serde(default)]
    pub state_secret: bool,
    /// The unit the value is in, by the value of another (enum) field (v2): an alarm's
    /// threshold is a percentage for `cpu` and seconds for `lb_latency`. The inspector
    /// shows it beside the field.
    #[serde(default)]
    pub units: Option<UnitsDef>,
    /// Where this field's values were stored before (v2), so a project saved then still
    /// loads with them: `{ provider = "aws", field = "statistic", skip = ["Average"] }`
    /// moves a saved AWS `statistic` here when this field has no value, except the values
    /// in `skip` (the old field's default, which every entity saved whether it meant it
    /// or not). Abstract fields only.
    #[serde(default)]
    pub moved_from: Option<MovedFrom>,
    /// On a `struct_list` item that is `required`: the requirement lapses in a row whose
    /// other item has one of these values (a rule's ports when its protocol is `all`).
    #[serde(default)]
    pub required_unless_item: Option<RequiredUnlessItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitsDef {
    /// An enum field of the same type whose value picks the unit.
    pub field: String,
    /// Option of that field -> unit, e.g. `cpu = "percent"`.
    pub values: IndexMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovedFrom {
    /// The provider whose provider field it was; absent for an abstract field.
    #[serde(default)]
    pub provider: Option<String>,
    pub field: String,
    /// Old values not carried over.
    #[serde(default)]
    pub skip: Vec<String>,
}

/// `required_unless_item = { item = "protocol", in = ["all", "icmp"] }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredUnlessItem {
    pub item: String,
    #[serde(rename = "in")]
    pub values: Vec<String>,
}

impl RequiredUnlessItem {
    /// Does the requirement lapse in this row?
    pub fn lapses(&self, row: &ttg_core::Record) -> bool {
        row.get(&self.item)
            .map(|v| v.display())
            .is_some_and(|v| self.values.contains(&v))
    }
}

impl FieldDef {
    /// The unit this field's value is in for an entity whose `units.field` has `selector`.
    pub fn unit_for(&self, selector: &str) -> Option<&str> {
        self.units.as_ref()?.values.get(selector).map(String::as_str)
    }
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
    /// A decimal number (v2), e.g. an alarm threshold of 0.5 seconds. Whole numbers are
    /// stored as integers, so a field that used to be an `int` keeps its saved values.
    Number,
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
    /// Providers this relation is meaningful for (v2). Other providers neither consume
    /// it nor warn that they cannot; empty means every provider.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Read only by the Kubernetes manifests export (v2): no Terraform mapping consumes
    /// the link, and like `calls` it adds no `depends_on` and no manual step.
    #[serde(default)]
    pub manifests: bool,
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
    /// What a client needs to reach this resource (v2): upper-case key -> argument
    /// source, resolved on this entity. The Kubernetes manifests export turns each one a
    /// linked workload uses into a `k8s_<slug>_<key>` output and an environment variable.
    #[serde(default)]
    pub connection: IndexMap<String, ArgSource>,
}

/// A definition-level diagnostic: when the condition holds the message is reported.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDef {
    pub when: Condition,
    /// Evaluate once per row of this field; `{item.<name>}` placeholders then work.
    #[serde(default)]
    pub for_each_field: Option<String>,
    /// `warning` (default), `error`, or `omit` — the provider cannot express this
    /// entity, so it is left out of that provider's export and the message is reported
    /// as a warning instead of blocking.
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
    /// Emit this block through an aliased configuration of the target provider
    /// (schema_version 2), e.g. `us_east_1` for CloudFront's certificates and Web ACLs.
    /// The alias must be declared in the provider definition.
    #[serde(default)]
    pub provider_alias: Option<String>,
    /// Emit one block per row of this `struct_list` / `string_list` field
    /// (schema_version 2). Sources inside may use `{ item = "..." }`.
    #[serde(default)]
    pub for_each_field: Option<String>,
    /// The row item whose value names each instance of a `for_each_field` block (its
    /// address is `<slug>_<key>_<value>`). Defaults to the row's `name` item, or the
    /// entry itself for a `string_list`; without either the row index is used.
    #[serde(default)]
    pub for_each_key: Option<String>,
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

impl BlockDef {
    /// A block with no conditions, rows or arguments: what a native resource or data
    /// source is made of.
    pub fn plain(key: &str, resource: &str) -> BlockDef {
        BlockDef {
            key: key.to_string(),
            resource: resource.to_string(),
            when: None,
            provider_alias: None,
            for_each_field: None,
            for_each_key: None,
            for_each_relation: None,
            for_each_target_type: None,
            args: IndexMap::new(),
            nested: Vec::new(),
        }
    }

    /// One block per row or per target rather than a single block.
    pub fn repeated(&self) -> bool {
        self.for_each_field.is_some() || self.for_each_relation.is_some()
    }
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
    /// Inside a `for_each_field` row: emit one nested block per entry of this list-valued
    /// sub-field of the row (v2). The inner row is the outer one with `value` set to the
    /// entry, so `{ item = "value" }` is the entry and the outer row's items stay readable.
    #[serde(default)]
    pub for_each_item: Option<String>,
    #[serde(default)]
    pub args: IndexMap<String, ArgSource>,
    #[serde(default)]
    pub nested: Vec<NestedBlockDef>,
}

/// A second step from each entity a relation reached (v2): from a CDN's origin load
/// balancer to the DNS Records that alias it. The entities the hop reaches replace the
/// first ones as the subjects of the source or condition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hop {
    pub relation: String,
    /// Follow the edges that point *at* the first-hop entity instead of away from it.
    #[serde(default)]
    pub incoming: bool,
    /// Only keep entities of this abstract type.
    #[serde(default)]
    pub target_type: Option<String>,
    /// Only keep entities whose host name (a DNS Record's fully qualified name, a CDN's
    /// custom domain) a TLS Certificate linked from the first-hop entity covers — its
    /// domain or one of its alternative names, `*.` wildcards one label deep.
    #[serde(default)]
    pub certificate_covers: bool,
}

/// Abstract types that have a host name `certificate_covers` can test.
pub const HOST_NAME_TYPES: &[&str] = &["dns_record", "cdn"];

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
    /// Only emit the output when this holds (v2): an attribute that only exists in one
    /// configuration, such as the secret RDS creates for a managed master password.
    #[serde(default)]
    pub when: Option<Condition>,
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
    /// `{ target_shares_ancestor = "virtual_network" }` — inside a `for_each_relation`
    /// block: the current target sits in the same container of that type as the entity
    /// itself (`absent = true` inverts) (v2). With `relation = "…"` the subject is every
    /// target of that relation instead of the current row's.
    Target(CondTarget),
    /// `{ setting = "kubernetes_manifests" }` — a project setting is on; `equals` /
    /// `not_equals` compare it instead (`equals = "false"`: the setting is off) (v2).
    Setting(CondSetting),
    /// `{ not = <condition> }` — the condition does not hold (v2).
    Not(CondNot),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondNot {
    pub not: Box<Condition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondSetting {
    /// One of `CONDITION_SETTINGS`.
    pub setting: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondTarget {
    pub target_shares_ancestor: String,
    /// Ask the question of this relation's targets rather than the current
    /// `for_each_relation` row, so it can be used outside a repeated block (v2).
    #[serde(default)]
    pub relation: Option<String>,
    /// Only consider targets of this abstract type (with `relation`).
    #[serde(default)]
    pub target_type: Option<String>,
    #[serde(default)]
    pub absent: bool,
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
    /// Look at edges that point *at* this entity instead of away from it (v2): "somebody
    /// logs to me". The entity at the other end is still filtered by `target_type`.
    #[serde(default)]
    pub incoming: bool,
    /// Only count targets of this abstract type (schema_version 2).
    #[serde(default)]
    pub target_type: Option<String>,
    /// Invert: holds when there is NO such target (v2).
    #[serde(default)]
    pub absent: bool,
    /// How many matching targets (or sources, with `incoming`) are needed for the
    /// condition to hold. Defaults to 1; `min_count = 2` is how a check says "more than
    /// one cluster logs to this group" (v2).
    #[serde(default)]
    pub min_count: Option<usize>,
    /// Only count targets whose abstract field / provider field matches `equals` /
    /// `not_equals` (v2). With neither, the field must be set and truthy.
    #[serde(default)]
    pub target_field: Option<String>,
    #[serde(default)]
    pub target_provider_field: Option<String>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    /// Take a second step from each target before counting (v2); `target_field` and
    /// `min_count` then apply to what the hop reaches.
    #[serde(default)]
    pub hop: Option<Hop>,
    /// Only count targets (or, with `incoming`, sources) for which this condition holds
    /// when it is evaluated on *them* (v2): "an app that links me as its role and has no
    /// execution role of its own". Needs `target_type`, which says whose fields and
    /// relations the condition may name.
    #[serde(default, rename = "where")]
    pub where_: Option<Box<Condition>>,
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
    /// `{ item = "source_group", ref_type = "kubernetes_cluster" }` — the `entity_ref`
    /// item points at an entity of this abstract type (v2). How a rule tells a
    /// security-group source from a cluster source when the two render differently.
    #[serde(default)]
    pub ref_type: Option<String>,
    /// Holds when the item has at least this many entries (a list item's length; a set
    /// scalar counts as one) (v2).
    #[serde(default)]
    pub min_count: Option<usize>,
    /// `{ item = "secret", linked = "reads" }` — the `entity_ref` item points at one of
    /// this entity's targets of that relation (v2), so a row can be held to the links
    /// that grant access to what it names.
    #[serde(default)]
    pub linked: Option<String>,
    /// Invert the test (v2): `{ item = "key", absent = true }` holds when the row has no
    /// value for the item.
    #[serde(default)]
    pub absent: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondField {
    pub field: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    /// The resolved value must start / not start with this string (v2). Ignored when
    /// `equals` / `not_equals` is also given.
    #[serde(default)]
    pub starts_with: Option<String>,
    #[serde(default)]
    pub not_starts_with: Option<String>,
    /// The resolved value must end / not end with this string (v2). Ignored when
    /// `equals` / `not_equals` is also given; takes priority over `starts_with` /
    /// `not_starts_with` if both are somehow given.
    #[serde(default)]
    pub ends_with: Option<String>,
    #[serde(default)]
    pub not_ends_with: Option<String>,
    /// Normalise the resolved string value (e.g. to the kebab-cased form the mapping
    /// actually emits) before comparing (v2).
    #[serde(default)]
    pub transform: Option<Transform>,
    /// Holds when the entity has no value for the field (defaults do not count) (v2).
    #[serde(default)]
    pub absent: bool,
    /// The value (as text) is one of these / none of these (v2): a provider's allowed
    /// retention periods. An unset field is none of them.
    #[serde(default)]
    pub one_of: Vec<String>,
    #[serde(default)]
    pub not_one_of: Vec<String>,
    /// Numeric comparisons against a number or another numeric field (v2). A value that
    /// is not a number satisfies none of them.
    #[serde(default)]
    pub less_than: Option<Box<Bound>>,
    #[serde(default)]
    pub at_most: Option<Box<Bound>>,
    #[serde(default)]
    pub greater_than: Option<Box<Bound>>,
    #[serde(default)]
    pub at_least: Option<Box<Bound>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CondProviderField {
    pub provider_field: String,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
    /// The resolved value must end / not end with this string (v2), e.g. an instance
    /// class override ending `.micro`. Ignored when `equals` / `not_equals` is also given.
    #[serde(default)]
    pub ends_with: Option<String>,
    #[serde(default)]
    pub not_ends_with: Option<String>,
    /// Holds when the entity has no value for the field (defaults do not count) (v2).
    #[serde(default)]
    pub absent: bool,
    /// See [`CondField`].
    #[serde(default)]
    pub one_of: Vec<String>,
    #[serde(default)]
    pub not_one_of: Vec<String>,
    #[serde(default)]
    pub less_than: Option<Box<Bound>>,
    #[serde(default)]
    pub at_most: Option<Box<Bound>>,
    #[serde(default)]
    pub greater_than: Option<Box<Bound>>,
    #[serde(default)]
    pub at_least: Option<Box<Bound>>,
}

/// Set membership and numeric comparisons of a [`CondField`] or [`CondProviderField`]
/// (v2). Checked after `equals` / `not_equals` and before the prefix / suffix tests; when
/// several are given they must all hold, so `at_least` with `at_most` is a range.
#[derive(Debug, Clone, Copy)]
pub struct Compare<'a> {
    pub one_of: &'a [String],
    pub not_one_of: &'a [String],
    pub less_than: Option<&'a Bound>,
    pub at_most: Option<&'a Bound>,
    pub greater_than: Option<&'a Bound>,
    pub at_least: Option<&'a Bound>,
}

impl<'a> Compare<'a> {
    pub fn is_empty(&self) -> bool {
        self.one_of.is_empty() && self.not_one_of.is_empty() && self.bounds().is_empty()
    }
    /// Every numeric bound with its operator name.
    pub fn bounds(&self) -> Vec<(&'static str, &'a Bound)> {
        [
            ("less_than", self.less_than),
            ("at_most", self.at_most),
            ("greater_than", self.greater_than),
            ("at_least", self.at_least),
        ]
        .into_iter()
        .filter_map(|(k, b)| b.map(|b| (k, b)))
        .collect()
    }
}

impl CondField {
    pub fn compare(&self) -> Compare<'_> {
        Compare {
            one_of: &self.one_of,
            not_one_of: &self.not_one_of,
            less_than: self.less_than.as_deref(),
            at_most: self.at_most.as_deref(),
            greater_than: self.greater_than.as_deref(),
            at_least: self.at_least.as_deref(),
        }
    }
}

impl CondProviderField {
    pub fn compare(&self) -> Compare<'_> {
        Compare {
            one_of: &self.one_of,
            not_one_of: &self.not_one_of,
            less_than: self.less_than.as_deref(),
            at_most: self.at_most.as_deref(),
            greater_than: self.greater_than.as_deref(),
            at_least: self.at_least.as_deref(),
        }
    }
}

/// The other side of a numeric comparison: a number, or another numeric field of the
/// same entity (`at_most = { field = "storage" }`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Bound {
    Number(f64),
    Field(BoundField),
    ProviderField(BoundProviderField),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundField {
    pub field: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundProviderField {
    pub provider_field: String,
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
    /// Read a `string_list` of `key=value` entries as an object (schema_version 2).
    /// Only valid on `field` / `provider_field`; see `kubernetes_node_pool`'s labels.
    Map,
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
    /// `{ for_each_field = "env", each = <source> }` — a list with one element per row of
    /// a table field, `each` resolved with that row as the `item` (v2).
    Rows(SrcRows),
    /// `{ raw = "..." }` — raw HCL expression, optionally with `refs` spliced in at
    /// `@name@`. Last resort.
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
    /// One sub-field of a `struct_list` field, as the list of its values across the rows
    /// (v2). Mutually exclusive with `wrap` and `transform`.
    #[serde(default)]
    pub column: Option<String>,
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
    /// One sub-field of a `struct_list` provider field, as the list of its values across
    /// the rows (v2). Mutually exclusive with `wrap` and `transform`.
    #[serde(default)]
    pub column: Option<String>,
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
    /// Attribute of the related resource. Empty (or absent, with `field` instead) means
    /// the block itself, which is what a `depends_on` list wants.
    #[serde(default)]
    pub attr: String,
    /// Follow the edges that point *at* this entity instead of away from it (v2): "the
    /// cluster that logs to me". `target_type` filters the *other* end's type, and
    /// containment never stands in for an incoming edge.
    #[serde(default)]
    pub incoming: bool,
    /// Read an abstract field of the related entity as a literal instead of referencing
    /// an attribute of its resource (v2). Alternative to `attr`; `transform` applies.
    /// This is how a name can be built from a neighbour without referring to its
    /// resource, and so without creating a dependency on it.
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub transform: Option<Transform>,
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
    /// Take a second step from each target and reference what it reaches instead (v2).
    #[serde(default)]
    pub hop: Option<Hop>,
    /// Resolve the target's own `connection` value of this key (v2): what a client needs
    /// to reach it, as its mapping declares it for the provider — a registry's host, a
    /// bucket's name. Alternative to `attr`, `field` and `block`.
    #[serde(default)]
    pub connection: Option<String>,
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
    /// For a repeated data block: the instance whose key equals this item of the current
    /// row (a security group rule's `prefix_list` names the lookup it uses).
    #[serde(default)]
    pub key_item: Option<String>,
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
/// A list built from a table field: one element per row (or per entry of a string_list,
/// whose row has the single item `value`), the rows `when` rejects left out, and rows
/// whose `each` resolves to nothing skipped. What `for_each_field` is to a block, this is
/// to an argument value — a container's environment inside `jsonencode`, say.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SrcRows {
    pub for_each_field: String,
    pub each: Box<ArgSource>,
    #[serde(default)]
    pub when: Option<Condition>,
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
    /// Sub-expressions spliced into `raw` wherever `@name@` appears, before the text is
    /// parsed as HCL (v2). Lets a mapping write an expression the source language has no
    /// form for — a `for_each` comprehension, say — without hard-coding local names.
    #[serde(default)]
    pub refs: IndexMap<String, ArgSource>,
}

impl SrcRaw {
    /// The `@name@` placeholders in the raw text, in order of appearance.
    pub fn placeholders(&self) -> Vec<&str> {
        self.raw
            .split('@')
            .skip(1)
            .step_by(2)
            .filter(|s| !s.is_empty())
            .collect()
    }
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
    /// Extra configurations of the same provider a mapping may send a block to with
    /// `provider_alias`. Each is emitted only when some emitted block uses it.
    #[serde(default)]
    pub aliases: Vec<ProviderAliasDef>,
    /// Container type every resource must be inside (Azure: `resource_group`).
    #[serde(default)]
    pub required_ancestor: Option<String>,
    /// Small side providers a mapping may draw a resource from (`hashicorp/random`).
    /// Each is added to `required_providers` only when an emitted block's resource type
    /// starts with its `prefix`.
    #[serde(default)]
    pub helper_providers: Vec<HelperProviderDef>,
    /// How the project's default tags reach this provider's output.
    #[serde(default)]
    pub default_tags: Option<DefaultTagsDef>,
}

/// A second configuration of the same provider, written as
/// `provider "<local_name>" { alias = "<name>" … }`. `args` override the arguments of the
/// normal provider block; everything else (nested blocks, default tags) is copied from it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAliasDef {
    /// Alias name; also the HCL identifier blocks reference as `<local_name>.<name>`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Provider-block arguments this configuration replaces, e.g. `region`.
    #[serde(default)]
    pub args: IndexMap<String, ArgSource>,
}

/// A provider a curated mapping borrows one resource type from, without it becoming a
/// target provider of its own (no provider block, no variables, no mappings).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperProviderDef {
    /// Registry source, e.g. `hashicorp/random`.
    pub source: String,
    pub version: String,
    /// Resource-type prefix that marks a block as belonging to this provider (`random_`).
    pub prefix: String,
    /// `required_providers` key; defaults to the last segment of `source`.
    #[serde(default)]
    pub local_name: Option<String>,
}

impl HelperProviderDef {
    pub fn local_name(&self) -> &str {
        self.local_name
            .as_deref()
            .unwrap_or_else(|| self.source.rsplit('/').next().unwrap_or(&self.source))
    }
    /// (namespace, name) of the registry source.
    pub fn source_parts(&self) -> (&str, &str) {
        self.source.split_once('/').unwrap_or(("hashicorp", &self.source))
    }
}

/// Where `Settings::tags` is written for one provider. Exactly one of `arg` (an argument
/// on the `provider` block, optionally inside `block`) and `resource_arg` (an argument
/// merged into every emitted resource whose schema has it) must be set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultTagsDef {
    /// Argument on the `provider` block, e.g. `default_labels`.
    #[serde(default)]
    pub arg: Option<String>,
    /// Nested block of the `provider` block that holds `arg`, e.g. `default_tags`.
    #[serde(default)]
    pub block: Option<String>,
    /// Argument merged into every emitted resource that accepts it, e.g. `tags`.
    #[serde(default)]
    pub resource_arg: Option<String>,
    /// Normalise keys and values to a label-safe form (lowercase, `-` for punctuation),
    /// which Google Cloud labels require.
    #[serde(default)]
    pub sanitize_labels: bool,
    /// The argument a resource carries its own tags in (`tags`, GCP `labels`). An
    /// entity's `owner` — and its `description`, where tag values can hold prose (not
    /// on label providers) — are merged into it on every emitted resource whose schema
    /// has it. Absent = the provider gets no per-entity tags.
    #[serde(default)]
    pub entity_arg: Option<String>,
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
