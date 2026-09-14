//! The Intermediate Representation: `Project`, `Node`, `Container`, `Edge`.
//!
//! See `docs/ARCHITECTURE.md` §4 for the serialized schema.

use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Identifier of a node or container. Human-readable (`subnet-3fa2b9c1`) so that diffs
/// of the project file stay meaningful.
pub type Id = String;

/// Identifier of a target provider (`aws`, `azure`, `gcp`).
pub type ProviderId = String;

/// A map of abstract field name -> value.
pub type Config = BTreeMap<String, Value>;

/// Which IaC tool the project targets. Codegen differences between the two are confined
/// to `ttg-codegen::tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Terraform,
    #[default]
    OpenTofu,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::OpenTofu, Tool::Terraform];
    pub fn display_name(self) -> &'static str {
        match self {
            Tool::Terraform => "Terraform",
            Tool::OpenTofu => "OpenTofu",
        }
    }
}

/// Remote/local state backend configuration. Rendered by the tool layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackendConfig {
    /// `local`, `s3`, `azurerm`, ...
    #[serde(rename = "type")]
    pub backend_type: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Settings {
    #[serde(default)]
    pub tool: Tool,
    #[serde(default = "default_provider")]
    pub target_provider: ProviderId,
    /// Values for provider-level variables declared in `definitions/providers/*.toml`
    /// (region, location, subscription id, ...).
    #[serde(default)]
    pub provider_settings: BTreeMap<ProviderId, BTreeMap<String, String>>,
    #[serde(default)]
    pub backend: Option<BackendConfig>,
    /// OpenTofu-only: emit a state `encryption` block. Ignored for Terraform.
    #[serde(default)]
    pub state_encryption: bool,
    /// Tags applied to every resource the project generates (cost allocation, ownership).
    /// How they reach the HCL is the provider definition's business: AWS `default_tags`,
    /// GCP `default_labels`, Azure a `tags` argument merged into each resource.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

fn default_provider() -> String {
    "aws".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tool: Tool::OpenTofu,
            target_provider: default_provider(),
            provider_settings: BTreeMap::new(),
            backend: None,
            state_encryption: false,
            tags: BTreeMap::new(),
        }
    }
}

/// Canvas position in world units (pixels at zoom 1.0). Stored as integers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl Default for Size {
    fn default() -> Self {
        Size { w: 480, h: 320 }
    }
}

/// Drawn size of a node (or a logical node) that has none of its own. The canvas and
/// the view-geometry helpers share it so both agree on where things are.
pub const NODE_SIZE: Size = Size { w: 176, h: 64 };

/// Extra provider arguments set directly on a generated block, keyed by argument name.
/// Values are JSON: scalars, lists, objects (maps / nested blocks), `{"$ref": {"entity":
/// "<id or name>", "attr": "id"}}` for a traversal to another resource, or `{"$raw":
/// "<hcl>"}` for a raw expression. Validated against the provider schema.
pub type ExtraArgs = serde_json::Map<String, serde_json::Value>;

/// provider id -> block key -> extra arguments.
pub type Extras = BTreeMap<ProviderId, BTreeMap<String, ExtraArgs>>;

/// A leaf resource on the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Node {
    pub id: Id,
    pub name: String,
    pub resource_type: String,
    #[serde(default)]
    pub config: Config,
    #[serde(default)]
    pub provider_config: BTreeMap<ProviderId, Config>,
    #[serde(default)]
    pub position: Position,
    /// Custom node size; absent means the default node size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    #[serde(default)]
    pub parent: Option<Id>,
    /// User-declared "this resource exists already / will be created by hand".
    #[serde(default)]
    pub manual: bool,
    /// Providers this entity is part of (empty = every provider). A provider layer is
    /// the abstract graph minus entities not tagged for it; see `Project::layer`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Extra provider arguments beyond what the mapping sets (schema-validated).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: Extras,
}

/// A resource that can hold other resources (VPC, Resource Group, Project).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Container {
    pub id: Id,
    pub name: String,
    pub container_type: String,
    #[serde(default)]
    pub config: Config,
    #[serde(default)]
    pub provider_config: BTreeMap<ProviderId, Config>,
    #[serde(default)]
    pub position: Position,
    #[serde(default)]
    pub size: Size,
    #[serde(default)]
    pub parent: Option<Id>,
    #[serde(default)]
    pub manual: bool,
    /// Providers this container is part of (empty = every provider).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Extra provider arguments beyond what the mapping sets (schema-validated).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: Extras,
}

/// Kinds of relationship an edge can express. Direction is always
/// *source depends on / references target*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    NetworkMembership,
    AttributeReference,
    IamBinding,
    /// Source is attached to / registered with target (route table -> subnet,
    /// load balancer -> instance, function -> its backing storage).
    Attachment,
    /// Source publishes messages to target (function -> queue).
    SendsTo,
    /// Source reads target's value (function -> secret, database -> password secret,
    /// secret -> database endpoint).
    Reads,
    /// Source writes its logs to target (function -> log group).
    LogsTo,
    /// Source is encrypted at rest with target's key (bucket -> encryption key).
    EncryptedWith,
    /// Messages the source could not deliver go to target (queue -> dead-letter queue).
    DeadLettersTo,
    DependsOn,
}

impl Relation {
    pub const ALL: [Relation; 10] = [
        Relation::NetworkMembership,
        Relation::AttributeReference,
        Relation::IamBinding,
        Relation::Attachment,
        Relation::SendsTo,
        Relation::Reads,
        Relation::LogsTo,
        Relation::EncryptedWith,
        Relation::DeadLettersTo,
        Relation::DependsOn,
    ];
    /// The identifier used in definition files.
    pub fn key(self) -> &'static str {
        match self {
            Relation::NetworkMembership => "network_membership",
            Relation::AttributeReference => "attribute_reference",
            Relation::IamBinding => "iam_binding",
            Relation::Attachment => "attachment",
            Relation::SendsTo => "sends_to",
            Relation::Reads => "reads",
            Relation::LogsTo => "logs_to",
            Relation::EncryptedWith => "encrypted_with",
            Relation::DeadLettersTo => "dead_letters_to",
            Relation::DependsOn => "depends_on",
        }
    }
    pub fn display_name(self) -> &'static str {
        match self {
            Relation::NetworkMembership => "Network membership",
            Relation::AttributeReference => "Attribute reference",
            Relation::IamBinding => "IAM binding",
            Relation::Attachment => "Attached to",
            Relation::SendsTo => "Sends to",
            Relation::Reads => "Reads",
            Relation::LogsTo => "Logs to",
            Relation::EncryptedWith => "Encrypted with",
            Relation::DeadLettersTo => "Dead-letters to",
            Relation::DependsOn => "Depends on (ordering only)",
        }
    }
    pub fn from_key(s: &str) -> Option<Relation> {
        Relation::ALL.into_iter().find(|r| r.key() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Edge {
    pub source: Id,
    pub target: Id,
    pub relation: Relation,
    /// Optional manual routing (which side of each node the line attaches to).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<EdgeLayout>,
    /// Providers this link is part of (empty = every provider).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
}

/// Where an edge attaches to its endpoints. Absent means automatic.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EdgeLayout {
    #[serde(default)]
    pub source: Anchor,
    #[serde(default)]
    pub target: Anchor,
}

/// One end of an edge: a side of the node and an offset along it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Anchor {
    /// `left` | `right` | `top` | `bottom`; `None` = automatic (face the other node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// Position along the side as a percentage from -100 (start) to 100 (end); 0 = centre.
    #[serde(default)]
    pub offset: i32,
}

/// Where a resource came from: the curated catalog or the raw provider schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    #[default]
    All,
    /// Only types from the curated catalog.
    Curated,
    /// Only `native:<provider>:<resource>` types.
    Native,
}

impl Origin {
    pub fn is_all(&self) -> bool {
        *self == Origin::All
    }
}

/// A saved canvas filter: which entities and links are shown. Purely visual; codegen
/// always sees the whole project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ViewFilter {
    /// Resource categories to show (empty = all).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub categories: BTreeSet<String>,
    /// Relation kinds to draw (empty = all).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub relations: BTreeSet<Relation>,
    /// Only show entities within `depth` links of this one (plus their containers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<Id>,
    #[serde(default = "default_depth")]
    pub depth: u32,
    /// Entities hidden explicitly.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hidden: BTreeSet<Id>,
    /// When non-empty, show only these entities (plus their containers). Used by
    /// "show this path" in the reachability overlay.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub only: BTreeSet<Id>,
    /// Draw containers (networks, resource groups, vaults). Off for architecture maps
    /// that group things their own way.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub containers: bool,
    /// Only entities on these provider layers (empty = every layer).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub providers: BTreeSet<ProviderId>,
    /// Curated types only, native (raw schema) types only, or both.
    #[serde(default, skip_serializing_if = "Origin::is_all")]
    pub origin: Origin,
    /// Only these abstract type ids (empty = all).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub types: BTreeSet<String>,
    /// Case-insensitive `*` / `?` glob on the display name (empty = all).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name_glob: String,
    /// Hide the structural links, so a view shows only its data-flow arrows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_edges: bool,
}

fn default_depth() -> u32 {
    1
}
fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}
fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for ViewFilter {
    fn default() -> Self {
        ViewFilter {
            categories: BTreeSet::new(),
            relations: BTreeSet::new(),
            focus: None,
            depth: 1,
            hidden: BTreeSet::new(),
            only: BTreeSet::new(),
            containers: true,
            providers: BTreeSet::new(),
            origin: Origin::All,
            types: BTreeSet::new(),
            name_glob: String::new(),
            hide_edges: false,
        }
    }
}

impl ViewFilter {
    /// True when the filter shows everything.
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
            && self.relations.is_empty()
            && self.focus.is_none()
            && self.hidden.is_empty()
            && self.only.is_empty()
            && self.containers
            && self.providers.is_empty()
            && self.origin.is_all()
            && self.types.is_empty()
            && self.name_glob.is_empty()
            && !self.hide_edges
    }
}

/// Case-insensitive glob with `*` (any run) and `?` (one character). Written out rather
/// than pulled in as a dependency: the patterns are display names, not paths.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have eaten too little.
    let (mut star, mut resume) = (None, 0usize);
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                resume = ti;
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ti += 1;
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some(s) => {
                    pi = s + 1;
                    resume += 1;
                    ti = resume;
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Positions and sizes a view keeps separately from the shared layout. Entities not
/// listed use the shared position, so an empty layout is "same as All, until moved".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ViewLayout {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub positions: BTreeMap<Id, Position>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sizes: BTreeMap<Id, Size>,
}

/// A grouping box drawn on a view: pure annotation, never exported. Entities are "in"
/// a group when their centre lies inside its box; dragging the group takes them along.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Group {
    pub id: Id,
    pub label: String,
    #[serde(default)]
    pub position: Position,
    #[serde(default)]
    pub size: Size,
    /// `#rrggbb`; a default palette colour when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// An annotation-only node on a view: something the diagram does not (and should not)
/// generate — a browser, a third-party API, one workload inside a cluster. Never
/// exported; flows may start and end at it, and it belongs to groups geometrically
/// like a resource does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Logical {
    pub id: Id,
    pub name: String,
    /// Short text drawn in the icon block, e.g. `WEB`.
    #[serde(default)]
    pub icon: String,
    /// One line under the name.
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub position: Position,
    #[serde(default = "default_logical_size")]
    pub size: Size,
}

fn default_logical_size() -> Size {
    NODE_SIZE
}

/// One end of a data-flow arrow: a resource, a group or a logical node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum FlowEnd {
    Entity { entity: Id },
    Group { group: Id },
    Logical { logical: Id },
}

impl FlowEnd {
    pub fn id(&self) -> &str {
        match self {
            FlowEnd::Entity { entity } => entity,
            FlowEnd::Group { group } => group,
            FlowEnd::Logical { logical } => logical,
        }
    }
}

/// What a note points at: anything a flow can attach to, or a flow itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum NoteAnchor {
    End(FlowEnd),
    Flow { flow: Id },
}

impl NoteAnchor {
    pub fn id(&self) -> &str {
        match self {
            NoteAnchor::End(e) => e.id(),
            NoteAnchor::Flow { flow } => flow,
        }
    }
}

/// A note box on a view: a title and a wrapped body explaining part of the picture.
/// Never exported. With an `anchor` it is drawn beside what it explains and `position`
/// is the offset from that thing's top-right corner; without one, `position` is the
/// note's own place on the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Note {
    pub id: Id,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub position: Position,
    #[serde(default = "default_note_size")]
    pub size: Size,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<NoteAnchor>,
}

fn default_note_size() -> Size {
    Size { w: 260, h: 120 }
}

/// A labelled data-flow arrow on a view: architecture-map annotation, never exported
/// and never a dependency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Flow {
    pub id: Id,
    pub from: FlowEnd,
    pub to: FlowEnd,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub dashed: bool,
    /// Position in the sequence, drawn as a badge at the arrow's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// `#rrggbb`; the default ink colour when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// A named, saved view shown as a tab above the canvas: a filter (what is shown), an
/// optional layout of its own (where things are), and annotations (groups, flows,
/// notes, logical nodes) that document it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct View {
    pub name: String,
    /// What this view is for, shown under the view bar and in its exported document.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub filter: ViewFilter,
    /// `Some` = this view positions things itself; `None` = shares the All layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<ViewLayout>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<Flow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logicals: Vec<Logical>,
    /// Show the legend panel over the canvas.
    #[serde(default, skip_serializing_if = "is_false")]
    pub legend: bool,
}

impl View {
    pub fn new(name: &str, filter: ViewFilter) -> Self {
        View {
            name: name.to_string(),
            description: String::new(),
            filter,
            layout: Some(ViewLayout::default()),
            groups: Vec::new(),
            flows: Vec::new(),
            notes: Vec::new(),
            logicals: Vec::new(),
            legend: false,
        }
    }
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
    pub fn group_mut(&mut self, id: &str) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }
    pub fn flow(&self, id: &str) -> Option<&Flow> {
        self.flows.iter().find(|f| f.id == id)
    }
    pub fn flow_mut(&mut self, id: &str) -> Option<&mut Flow> {
        self.flows.iter_mut().find(|f| f.id == id)
    }
    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }
    pub fn note_mut(&mut self, id: &str) -> Option<&mut Note> {
        self.notes.iter_mut().find(|n| n.id == id)
    }
    pub fn logical(&self, id: &str) -> Option<&Logical> {
        self.logicals.iter().find(|l| l.id == id)
    }
    pub fn logical_mut(&mut self, id: &str) -> Option<&mut Logical> {
        self.logicals.iter_mut().find(|l| l.id == id)
    }
    /// Flows in the order they should be read: numbered steps first, then the rest in
    /// the order they were added.
    pub fn flows_in_step_order(&self) -> Vec<&Flow> {
        let mut out: Vec<&Flow> = self.flows.iter().collect();
        out.sort_by_key(|f| f.step.unwrap_or(u32::MAX));
        out
    }
}

/// The whole diagram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Project {
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub containers: BTreeMap<Id, Container>,
    #[serde(default)]
    pub nodes: BTreeMap<Id, Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Saved canvas views (filters). Optional; absent in files written by older versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<View>,
}

impl Default for Project {
    fn default() -> Self {
        Self::new("untitled")
    }
}

/// A uniform read-only view over either a node or a container. Codegen and validation
/// treat both the same way except for containment.
#[derive(Debug, Clone, Copy)]
pub struct EntityRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub resource_type: &'a str,
    pub config: &'a Config,
    pub provider_config: &'a BTreeMap<ProviderId, Config>,
    pub parent: Option<&'a str>,
    pub manual: bool,
    pub is_container: bool,
    pub position: Position,
    pub extra: &'a Extras,
}

impl<'a> EntityRef<'a> {
    /// Look up an abstract field value.
    pub fn field(&self, name: &str) -> Option<&'a Value> {
        if name == "name" {
            return None;
        }
        self.config.get(name)
    }
    /// Look up a provider-specific field value.
    pub fn provider_field(&self, provider: &str, name: &str) -> Option<&'a Value> {
        self.provider_config.get(provider).and_then(|c| c.get(name))
    }
}

impl Project {
    pub fn new(name: &str) -> Self {
        Project {
            schema_version: crate::SCHEMA_VERSION,
            name: name.to_string(),
            settings: Settings::default(),
            containers: BTreeMap::new(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            views: Vec::new(),
        }
    }

    /// Generate a fresh id with a readable prefix, e.g. `subnet-3fa2b9c1`.
    pub fn fresh_id(&self, prefix: &str) -> Id {
        loop {
            let u = uuid::Uuid::new_v4().simple().to_string();
            let id = format!("{}-{}", prefix, &u[..8]);
            if !self.nodes.contains_key(&id) && !self.containers.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn entity(&self, id: &str) -> Option<EntityRef<'_>> {
        if let Some(n) = self.nodes.get(id) {
            return Some(EntityRef {
                id: &n.id,
                name: &n.name,
                resource_type: &n.resource_type,
                config: &n.config,
                provider_config: &n.provider_config,
                parent: n.parent.as_deref(),
                manual: n.manual,
                is_container: false,
                position: n.position,
                extra: &n.extra,
            });
        }
        self.containers.get(id).map(|c| EntityRef {
            id: &c.id,
            name: &c.name,
            resource_type: &c.container_type,
            config: &c.config,
            provider_config: &c.provider_config,
            parent: c.parent.as_deref(),
            manual: c.manual,
            is_container: true,
            position: c.position,
            extra: &c.extra,
        })
    }

    /// All entities, containers first (outer before inner), then nodes, each sorted by id.
    pub fn entities(&self) -> Vec<EntityRef<'_>> {
        let mut out: Vec<EntityRef<'_>> = Vec::new();
        // Containers ordered by depth so ancestors precede descendants.
        let mut cs: Vec<&Container> = self.containers.values().collect();
        cs.sort_by_key(|c| (self.depth_of(&c.id), c.id.clone()));
        for c in cs {
            out.push(self.entity(&c.id).unwrap());
        }
        for n in self.nodes.values() {
            out.push(self.entity(&n.id).unwrap());
        }
        out
    }

    pub fn contains(&self, id: &str) -> bool {
        self.nodes.contains_key(id) || self.containers.contains_key(id)
    }

    pub fn parent_of(&self, id: &str) -> Option<&str> {
        self.entity(id).and_then(|e| e.parent)
    }

    /// Nesting depth (0 = top level). Cycles are guarded by a step limit.
    pub fn depth_of(&self, id: &str) -> usize {
        let mut d = 0;
        let mut cur = self.parent_of(id);
        while let Some(p) = cur {
            d += 1;
            if d > 64 {
                break;
            }
            cur = self.parent_of(p);
        }
        d
    }

    /// Chain of ancestors, nearest first.
    pub fn ancestors(&self, id: &str) -> Vec<&Container> {
        let mut out = Vec::new();
        let mut cur = self.parent_of(id);
        while let Some(p) = cur {
            if out.len() > 64 {
                break;
            }
            match self.containers.get(p) {
                Some(c) => {
                    out.push(c);
                    cur = c.parent.as_deref();
                }
                None => break,
            }
        }
        out
    }

    /// Nearest ancestor container of the given abstract type.
    pub fn ancestor_of_type(&self, id: &str, container_type: &str) -> Option<&Container> {
        self.ancestors(id)
            .into_iter()
            .find(|c| c.container_type == container_type)
    }

    /// Direct children (nodes and containers) of a container, sorted by id.
    pub fn children_of(&self, container_id: &str) -> Vec<Id> {
        let mut out: Vec<Id> = self
            .nodes
            .values()
            .filter(|n| n.parent.as_deref() == Some(container_id))
            .map(|n| n.id.clone())
            .chain(
                self.containers
                    .values()
                    .filter(|c| c.parent.as_deref() == Some(container_id))
                    .map(|c| c.id.clone()),
            )
            .collect();
        out.sort();
        out
    }

    /// All descendants (transitive), containers and nodes.
    pub fn descendants_of(&self, container_id: &str) -> Vec<Id> {
        let mut out = Vec::new();
        let mut stack = vec![container_id.to_string()];
        while let Some(cur) = stack.pop() {
            for c in self.children_of(&cur) {
                if self.containers.contains_key(&c) {
                    stack.push(c.clone());
                }
                out.push(c);
            }
        }
        out
    }

    /// True if `ancestor` is a (transitive) ancestor of `id`.
    pub fn is_ancestor(&self, ancestor: &str, id: &str) -> bool {
        self.ancestors(id).iter().any(|c| c.id == ancestor)
    }

    /// Outgoing edges (this entity as source).
    pub fn edges_from<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.source == id)
    }

    /// Incoming edges (this entity as target).
    pub fn edges_to<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges.iter().filter(move |e| e.target == id)
    }

    /// Add an edge if it does not already exist. Returns true if added.
    pub fn add_edge(&mut self, source: &str, target: &str, relation: Relation) -> bool {
        if self
            .edges
            .iter()
            .any(|e| e.source == source && e.target == target && e.relation == relation)
        {
            return false;
        }
        self.edges.push(Edge {
            source: source.to_string(),
            target: target.to_string(),
            relation,
            layout: None,
            providers: Vec::new(),
        });
        true
    }

    /// Is this entity part of the `provider` layer? `type_on` says whether its abstract
    /// type exists on that provider (provider-scoped types are auto-tagged). An entity
    /// tagged for other providers only, or whose type does not exist there, is off-layer.
    pub fn entity_on_layer(&self, id: &str, provider: &str, type_on: &dyn Fn(&str) -> bool) -> bool {
        let Some(e) = self.entity(id) else { return false };
        let tags = if let Some(n) = self.nodes.get(id) {
            &n.providers
        } else {
            &self.containers[id].providers
        };
        (tags.is_empty() || tags.iter().any(|p| p == provider)) && type_on(e.resource_type)
    }

    pub fn edge_on_layer(&self, e: &Edge, provider: &str, type_on: &dyn Fn(&str) -> bool) -> bool {
        (e.providers.is_empty() || e.providers.iter().any(|p| p == provider))
            && self.entity_on_layer(&e.source, provider, type_on)
            && self.entity_on_layer(&e.target, provider, type_on)
    }

    /// Ids of every entity on the `provider` layer.
    pub fn layer_members(&self, provider: &str, type_on: &dyn Fn(&str) -> bool) -> BTreeSet<Id> {
        self.entities()
            .iter()
            .filter(|e| self.entity_on_layer(e.id, provider, type_on))
            .map(|e| e.id.to_string())
            .collect()
    }

    /// The `provider` layer as a stand-alone project: off-layer entities and links are
    /// dropped, and children of a dropped container are re-parented to their nearest
    /// kept ancestor (a provider-only container is just grouping elsewhere). Everything
    /// downstream (codegen, diagnostics, reachability) works on this.
    pub fn layer(&self, provider: &str, type_on: &dyn Fn(&str) -> bool) -> Project {
        let keep = self.layer_members(provider, type_on);
        let mut out = self.clone();
        out.nodes.retain(|id, _| keep.contains(id));
        out.containers.retain(|id, _| keep.contains(id));
        let nearest = |start: Option<&str>| -> Option<Id> {
            let mut cur: Option<String> = start.map(|s| s.to_string());
            while let Some(c) = cur {
                if keep.contains(&c) {
                    return Some(c);
                }
                cur = self.parent_of(&c).map(|s| s.to_string());
            }
            None
        };
        for n in out.nodes.values_mut() {
            n.parent = nearest(n.parent.as_deref());
        }
        for c in out.containers.values_mut() {
            c.parent = nearest(c.parent.as_deref());
        }
        out.edges.retain(|e| self.edge_on_layer(e, provider, type_on));
        out
    }

    /// Remove an entity and everything that references it. Removing a container
    /// re-parents its children to the container's own parent (it does not delete them).
    pub fn remove_entity(&mut self, id: &str) {
        let new_parent = self.parent_of(id).map(|s| s.to_string());
        if self.containers.remove(id).is_some() {
            for n in self.nodes.values_mut() {
                if n.parent.as_deref() == Some(id) {
                    n.parent = new_parent.clone();
                }
            }
            for c in self.containers.values_mut() {
                if c.parent.as_deref() == Some(id) {
                    c.parent = new_parent.clone();
                }
            }
        }
        self.nodes.remove(id);
        self.edges.retain(|e| e.source != id && e.target != id);
        for v in &mut self.views {
            v.filter.hidden.remove(id);
            v.filter.only.remove(id);
            if v.filter.focus.as_deref() == Some(id) {
                v.filter.focus = None;
            }
            if let Some(l) = &mut v.layout {
                l.positions.remove(id);
                l.sizes.remove(id);
            }
            v.flows.retain(|f| f.from.id() != id && f.to.id() != id);
            // Notes survive; they just stop pointing at something that is gone.
            for n in &mut v.notes {
                if n.anchor.as_ref().is_some_and(|a| a.id() == id) {
                    n.anchor = None;
                }
            }
        }
    }

    /// Set the parent of an entity. Rejects (returns false) if it would create a
    /// containment cycle or the parent is not a container.
    pub fn set_parent(&mut self, id: &str, parent: Option<&str>) -> bool {
        if let Some(p) = parent {
            if p == id || !self.containers.contains_key(p) || self.is_ancestor(id, p) {
                return false;
            }
        }
        if let Some(n) = self.nodes.get_mut(id) {
            n.parent = parent.map(|s| s.to_string());
            return true;
        }
        if let Some(c) = self.containers.get_mut(id) {
            c.parent = parent.map(|s| s.to_string());
            return true;
        }
        false
    }

    /// Slugified HCL-safe local name for an entity (`My Bucket!` -> `my_bucket`).
    pub fn hcl_name(&self, id: &str) -> String {
        self.entity(id).map(|e| slugify(e.name)).unwrap_or_default()
    }
}

/// Turn a display name into a valid HCL identifier: lowercase, `[a-z0-9_]`, starts with a
/// letter. Empty names become `unnamed`.
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    let mut last_us = false;
    for ch in name.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            s.push(c);
            last_us = false;
        } else if !last_us && !s.is_empty() {
            s.push('_');
            last_us = true;
        }
    }
    while s.ends_with('_') {
        s.pop();
    }
    if s.is_empty() {
        return "unnamed".into();
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

impl EntityRef<'_> {
    /// Extra arguments for one generated block on a provider.
    pub fn extra_args(&self, provider: &str, block: &str) -> Option<&ExtraArgs> {
        self.extra.get(provider)?.get(block)
    }
}

impl Project {
    /// Mutable extra arguments for one block of an entity (created on demand).
    pub fn extra_args_mut(&mut self, id: &str, provider: &str, block: &str) -> Option<&mut ExtraArgs> {
        let extras = if let Some(n) = self.nodes.get_mut(id) {
            &mut n.extra
        } else {
            &mut self.containers.get_mut(id)?.extra
        };
        Some(
            extras
                .entry(provider.to_string())
                .or_default()
                .entry(block.to_string())
                .or_default(),
        )
    }

    /// Drop empty extra-argument maps so files stay tidy.
    pub fn prune_extras(&mut self, id: &str) {
        let extras = if let Some(n) = self.nodes.get_mut(id) {
            &mut n.extra
        } else if let Some(c) = self.containers.get_mut(id) {
            &mut c.extra
        } else {
            return;
        };
        for m in extras.values_mut() {
            m.retain(|_, a| !a.is_empty());
        }
        extras.retain(|_, m| !m.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug() {
        assert_eq!(slugify("My Bucket!"), "my_bucket");
        assert_eq!(slugify("  web-01 "), "web_01");
        assert_eq!(slugify("1abc"), "_1abc");
        assert_eq!(slugify("---"), "unnamed");
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("jobs*", "Jobs queue"));
        assert!(glob_match("*queue", "jobs QUEUE"));
        assert!(glob_match("*job*", "the jobs db"));
        assert!(glob_match("db-?", "db-a"));
        assert!(glob_match("runner", "runner"));
        assert!(!glob_match("db-?", "db-ab"));
        assert!(!glob_match("jobs*", "the jobs db"));
        assert!(!glob_match("*queue", "queue of jobs"));
        // Backtracking: the first `*` must give characters back.
        assert!(glob_match("*ab*c", "xxabxxabc"));
    }

    /// Every annotation and filter field survives a trip through the project file, and
    /// a file written before they existed still loads.
    #[test]
    fn view_documents_round_trip() {
        let mut p = Project::new("t");
        let mut v = View::new("map", ViewFilter::default());
        v.description = "what this view is for".into();
        v.legend = true;
        v.filter.providers = ["aws".to_string()].into_iter().collect();
        v.filter.origin = Origin::Native;
        v.filter.types = ["subnet".to_string()].into_iter().collect();
        v.filter.name_glob = "job*".into();
        v.filter.hide_edges = true;
        v.groups.push(Group {
            id: "g1".into(),
            label: "box".into(),
            position: Position { x: 1, y: 2 },
            size: Size { w: 300, h: 200 },
            color: Some("#5b7fb5".into()),
        });
        v.logicals.push(Logical {
            id: "l1".into(),
            name: "users' browser".into(),
            icon: "WEB".into(),
            subtitle: "outside the cloud".into(),
            position: Position { x: 3, y: 4 },
            size: NODE_SIZE,
        });
        v.flows.push(Flow {
            id: "f1".into(),
            from: FlowEnd::Logical { logical: "l1".into() },
            to: FlowEnd::Group { group: "g1".into() },
            label: "HTTPS".into(),
            dashed: true,
            step: Some(2),
            color: Some("#c95555".into()),
        });
        v.notes.push(Note {
            id: "n1".into(),
            title: "why".into(),
            body: "because".into(),
            position: Position { x: 5, y: 6 },
            size: Size { w: 260, h: 120 },
            anchor: Some(NoteAnchor::Flow { flow: "f1".into() }),
        });
        p.views.push(v.clone());
        let text = serde_json::to_string_pretty(&p).unwrap();
        let back: Project = serde_json::from_str(&text).unwrap();
        assert_eq!(back.views[0], v);
        // The untagged endpoint enums keep their variant, not just their id.
        assert!(matches!(back.views[0].flows[0].from, FlowEnd::Logical { .. }));
        assert!(matches!(
            back.views[0].notes[0].anchor,
            Some(NoteAnchor::Flow { .. })
        ));
        // A view as written by an older build (no annotations, no new filter keys).
        let old: View = serde_json::from_str(r#"{"name":"old","filter":{"depth":1}}"#).unwrap();
        assert!(old.notes.is_empty() && old.logicals.is_empty() && !old.legend);
        assert!(old.filter.is_empty());
    }

    #[test]
    fn containment_cycle_rejected() {
        let mut p = Project::new("t");
        for id in ["a", "b"] {
            p.containers.insert(
                id.into(),
                Container {
                    id: id.into(),
                    name: id.into(),
                    container_type: "virtual_network".into(),
                    config: Default::default(),
                    provider_config: Default::default(),
                    position: Default::default(),
                    size: Default::default(),
                    parent: None,
                    manual: false,
                    providers: Vec::new(),
                    extra: Default::default(),
                },
            );
        }
        assert!(p.set_parent("b", Some("a")));
        assert!(!p.set_parent("a", Some("b")));
        assert_eq!(p.children_of("a"), vec!["b".to_string()]);
    }
}
