//! The rmcp server: tool definitions and the HTTP runtime thread.
//!
//! Every tool forwards an [`AgentCommand`] to the UI thread and waits for the reply.
//! The server itself holds no project state, so any number of sessions can share it.

use super::approvals::Approvals;
use super::{oauth, AgentCommand, AgentReply, Heartbeat, ServerEvent, Started};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock as Content, ErrorData as McpError, Implementation,
        ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
        ResourceUpdatedNotification, ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo,
        ServerNotification, SubscribeRequestParams, UnsubscribeRequestParams,
    },
    schemars,
    service::{Peer, RequestContext},
    tool, tool_handler, tool_router, RoleServer, ServerHandler,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// `(session, peer, uri)` for every `resources/subscribe` still in force.
type Subscribers = Arc<Mutex<Vec<(u64, Peer<RoleServer>, String)>>>;

static SESSIONS: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct TtgServer {
    tx: mpsc::Sender<(AgentCommand, AgentReply)>,
    ctx: egui::Context,
    subs: Subscribers,
    /// When the UI last drained the queue, so a blocked app is refused rather than
    /// queued behind whatever is blocking it.
    beat: Arc<Heartbeat>,
    /// One `TtgServer` per client session.
    session: u64,
    /// Fingerprint of the loaded definitions, quoted in `serverInfo` (R3.20).
    catalog_hash: String,
    /// Approval tickets, read directly for `approval_status` (no trip through the UI).
    approvals: Arc<Approvals>,
    /// Read by the `#[tool_handler]`-generated `call_tool` / `list_tools`.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Resources the server exposes alongside its tools.
const RESOURCES: &[(&str, &str, &str, &str)] = &[
    (
        "ttg://project",
        "project",
        "The open project as JSON (same shape as the .ttg.json file)",
        "application/json",
    ),
    (
        "ttg://project/summary",
        "project summary",
        "Counts, tool, target provider, path, dirty flag, revision, diagnostics totals",
        "application/json",
    ),
    (
        "ttg://diagnostics",
        "diagnostics",
        "Current diagnostics for the target provider, plus what the other providers would refuse",
        "application/json",
    ),
    (
        "ttg://catalog",
        "catalog",
        "Every abstract resource type with mapping status",
        "application/json",
    ),
    ("ttg://docs/readme", "README", "User guide", "text/markdown"),
    (
        "ttg://docs/mapping-format",
        "mapping format",
        "How resource definitions (TOML) map abstract types to provider resources",
        "text/markdown",
    ),
    (
        "ttg://docs/architecture",
        "architecture",
        "Crate layout and data flow",
        "text/markdown",
    ),
];

fn doc(uri: &str) -> Option<&'static str> {
    match uri {
        "ttg://docs/readme" => Some(include_str!("../../../../README.md")),
        "ttg://docs/mapping-format" => Some(include_str!("../../../../docs/MAPPING_FORMAT.md")),
        "ttg://docs/architecture" => Some(include_str!("../../../../docs/ARCHITECTURE.md")),
        _ => None,
    }
}

/// Build a write command from a tool name and its JSON arguments (for `project_apply`).
pub fn command_from_json(tool: &str, args: serde_json::Value) -> Result<AgentCommand, String> {
    fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, String> {
        serde_json::from_value(v).map_err(|e| format!("bad arguments: {e}"))
    }
    let args = if args.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args
    };
    Ok(match tool {
        "entity_add" => {
            let a: EntityAddArgs = parse(args)?;
            AgentCommand::EntityAdd {
                type_id: a.type_id,
                name: a.name,
                parent: a.parent,
                x: a.x,
                y: a.y,
                providers: a.providers,
                verbose: a.verbose,
            }
        }
        "entity_update" => parse::<EntityUpdateArgs>(args)?.into_command()?,
        "entity_move" => {
            let a: MoveArgs = parse(args)?;
            AgentCommand::EntityMove {
                entity: a.entity,
                x: a.x,
                y: a.y,
                view: a.view,
            }
        }
        "entity_resize" => {
            let a: ResizeArgs = parse(args)?;
            AgentCommand::EntityResize {
                entity: a.entity,
                w: a.w,
                h: a.h,
                view: a.view,
            }
        }
        "entity_set_parent" => {
            let a: SetParentArgs = parse(args)?;
            AgentCommand::EntitySetParent {
                entity: a.entity,
                parent: a.parent,
            }
        }
        "entity_delete" => {
            let a: EntitiesArgs = parse(args)?;
            AgentCommand::EntityDelete { entities: a.entities }
        }
        "link_add" => parse::<LinkAddArgs>(args)?.into_command()?,
        "link_remove" => {
            let a: LinkRemoveArgs = parse(args)?;
            AgentCommand::LinkRemove {
                source: a.source,
                target: a.target,
                relation: a.relation,
            }
        }
        "selection_set" => {
            let a: EntitiesArgs = parse(args)?;
            AgentCommand::SelectionSet { entities: a.entities }
        }
        "view_set" => {
            let a: ViewSetArgs = parse(args)?;
            AgentCommand::ViewSet { filter: a.filter }
        }
        "view_save" => {
            let a: ViewSaveArgs = parse(args)?;
            AgentCommand::ViewSave {
                name: a.name,
                replace: a.replace.unwrap_or(false),
            }
        }
        "view_delete" => {
            let a: NameArgs = parse(args)?;
            AgentCommand::ViewDelete { name: a.name }
        }
        "view_activate" => {
            let a: NameArgs = parse(args)?;
            AgentCommand::ViewActivate { name: a.name }
        }
        "view_group_add" => {
            let a: GroupAddArgs = parse(args)?;
            AgentCommand::GroupAdd {
                view: a.view,
                label: a.label,
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
                color: a.color,
            }
        }
        "view_flow_add" => {
            let a: FlowAddArgs = parse(args)?;
            AgentCommand::FlowAdd {
                view: a.view,
                from: a.from,
                to: a.to,
                label: a.label.unwrap_or_default(),
                dashed: a.dashed.unwrap_or(false),
                step: a.step,
                color: a.color,
                show_hidden: a.show_hidden.unwrap_or(false),
                data: a.data,
            }
        }
        "view_note_add" => {
            let a: NoteAddArgs = parse(args)?;
            AgentCommand::NoteAdd {
                view: a.view,
                title: a.title,
                body: a.body.unwrap_or_default(),
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
                anchor: a.anchor,
            }
        }
        "view_logical_add" => {
            let a: LogicalAddArgs = parse(args)?;
            AgentCommand::LogicalAdd {
                view: a.view,
                name: a.name,
                icon: a.icon,
                subtitle: a.subtitle,
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
            }
        }
        "view_update" => {
            let a: ViewUpdateArgs = parse(args)?;
            AgentCommand::ViewUpdate {
                view: a.view,
                name: a.name,
                description: a.description,
                legend: a.legend,
                filter: a.filter,
            }
        }
        "view_annotation_remove" => {
            let a: KeyArgs = parse(args)?;
            AgentCommand::AnnotationRemove {
                view: a.view,
                key: a.key,
            }
        }
        "layout_tidy" => {
            let a: TidyArgs = parse(args)?;
            AgentCommand::LayoutTidy {
                container: a.container,
                view: a.view,
                by: a.by,
            }
        }
        // View tools: flows generated from links, notes put back beside their anchors.
        "view_generate" => {
            let a: ViewGenerateArgs = parse(args)?;
            AgentCommand::ViewGenerate {
                view: a.view,
                kind: a.kind,
                replace: a.replace.unwrap_or(false),
            }
        }
        "view_arrange_notes" => {
            let a: ViewNameArg = parse(args)?;
            AgentCommand::ViewArrangeNotes { view: a.view }
        }
        "layout_align" => {
            let a: AlignArgs = parse(args)?;
            AgentCommand::LayoutAlign { how: a.how }
        }
        "layout_distribute" => {
            let a: DistributeArgs = parse(args)?;
            AgentCommand::LayoutDistribute {
                horizontal: a.horizontal,
            }
        }
        "settings_set" => {
            let a: SettingsArgs = parse(args)?;
            a.into_command()?
        }
        other => {
            return Err(format!(
                "`{other}` cannot be used inside project_apply (only diagram writes: entity_*, link_*, layout_*, selection_set, settings_set and the view_* writes — not the view_get / view_fit / view_export reads)"
            ))
        }
    })
}

fn ok_json(v: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
    CallToolResult::success(vec![Content::text(text)])
}

fn fail(msg: impl Into<String>) -> CallToolResult {
    let msg = msg.into();
    // An error with no text tells the agent nothing (and some clients show a bare
    // `isError`); there is always something to say.
    let msg = if msg.trim().is_empty() {
        "the app reported an error without a message; check project_summary and diagnostics".to_string()
    } else {
        msg
    };
    CallToolResult::error(vec![Content::text(msg)])
}

// ------------------------------------------------------------------ parameter types

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeArgs {
    #[schemars(description = "Abstract type id, e.g. `subnet`, `function`, `event_queue`")]
    pub type_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntityArgs {
    #[schemars(description = "Entity id or display name (case-insensitive)")]
    pub entity: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PreviewArgs {
    #[schemars(
        description = "Provider id (`aws` / `azure` / `gcp`); defaults to the project's target provider"
    )]
    pub provider: Option<String>,
    #[schemars(
        description = "Include the Kubernetes manifests (k8s/…) whatever settings.kubernetes_manifests says; false leaves them out"
    )]
    pub k8s: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntityAddArgs {
    #[schemars(description = "Abstract type id from catalog_types, e.g. `subnet`")]
    pub type_id: String,
    #[schemars(description = "Display name; must be unique after slugifying. Defaults to the type name")]
    pub name: Option<String>,
    #[schemars(
        description = "Container to place it in (id or name). Must be an allowed parent for the type"
    )]
    pub parent: Option<String>,
    #[schemars(description = "Canvas x; defaults to a free spot inside the parent or right of the diagram")]
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[schemars(
        description = "Provider layers this entity belongs to, e.g. [\"azure\"]; omit for every provider"
    )]
    pub providers: Option<Vec<String>>,
    #[schemars(
        description = "false: reply with only status, id and changed instead of the whole entity (settings_set { verbose } sets the default for the session)"
    )]
    pub verbose: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntityUpdateArgs {
    #[schemars(description = "Entity id or display name. Give this, or `select` to update many at once")]
    pub entity: Option<String>,
    #[schemars(
        description = "Bulk update: apply the same config / provider_config / manual / providers / extra / classification / description / owner to every entity this matches, as ONE undo step. All-or-nothing: if any match refuses a value the whole call changes nothing and the reply lists every refusal. Cannot rename. An empty or non-matching selection is refused"
    )]
    pub select: Option<SelectArgs>,
    #[schemars(description = "New display name (single entity only)")]
    pub name: Option<String>,
    #[schemars(
        description = "Abstract field values keyed by field name (see catalog_type). Types are checked against the definition"
    )]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(description = "Provider-specific fields: { \"aws\": { \"bucket_name\": \"...\" } }")]
    pub provider_config: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(description = "Flag as external / managed by hand")]
    pub manual: Option<bool>,
    #[schemars(description = "Provider layers this entity belongs to; [] = every provider")]
    pub providers: Option<Vec<String>>,
    #[schemars(
        description = "Extra provider arguments merged into the generated block, keyed by argument name (see schema_show). Values: JSON scalars/lists/objects, {\"$ref\": {\"entity\": \"<id or name>\", \"attr\": \"id\"}} for a reference to the target's primary block (add \"block\": \"<key>\" to address one of its secondary blocks, e.g. object_storage's \"versioning\" block on AWS, and \"key\": \"<row key>\" to pick one instance of a repeated block, e.g. {\"entity\": \"ecr\", \"block\": \"repo\", \"key\": \"zipos-web\", \"attr\": \"repository_url\"}; a native data source is referenced the same way), {\"$raw\": \"<hcl>\"} for raw HCL, whose addresses are checked against what the export generates (add \"refs\": {\"name\": {\"$ref\": …}} and write @name@ in the HCL to splice a reference in). null on an argument the mapping sets removes it from the generated block (on a nested block, the block); null on anything else removes the extra argument; {\"$restore\": true} drops an override or removal and brings the mapping's own value back. For native resources and data sources this is where every argument goes"
    )]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(description = "Provider the extra arguments are for; defaults to the target provider")]
    pub extra_provider: Option<String>,
    #[schemars(description = "Block key the extra arguments apply to; defaults to the primary block")]
    pub extra_block: Option<String>,
    #[schemars(
        description = "How sensitive the data it holds is: public | internal | confidential | personal | payment; \"none\" or \"\" clears it"
    )]
    pub classification: Option<String>,
    #[schemars(
        description = "Why the resource exists, in a sentence or two. Emitted as an HCL comment above its blocks and a Description tag on AWS / Azure (cut to 256 characters); \"\" clears it"
    )]
    pub description: Option<String>,
    #[schemars(
        description = "Team or person who looks after it. Emitted as an Owner tag (AWS, Azure) or owner label (Google Cloud); \"\" clears it"
    )]
    pub owner: Option<String>,
    #[schemars(
        description = "false: reply with only status, id and changed instead of the whole entity (settings_set { verbose } sets the default for the session)"
    )]
    pub verbose: Option<bool>,
    #[schemars(
        description = "Named environment (settings.environments) the config / provider_config values are for: they override the entity's own values there only; null drops that environment's value (back to the base). Works with `entity` or `select`; with it only config, provider_config and present are taken"
    )]
    pub environment: Option<String>,
    #[schemars(
        description = "With `environment`: false leaves the entity (and its links) out of that environment — the export counts it there — true puts it back"
    )]
    pub present: Option<bool>,
}

impl EntityUpdateArgs {
    /// The single-entity or the bulk command these arguments ask for.
    fn into_command(self) -> Result<AgentCommand, String> {
        // One environment's values: their own command (`exec/environments.rs`).
        if let Some(environment) = self.environment {
            let others = self.name.is_some()
                || self.manual.is_some()
                || self.providers.is_some()
                || self.extra.is_some()
                || self.classification.is_some()
                || self.description.is_some()
                || self.owner.is_some();
            if others {
                return Err("with `environment`, entity_update takes only config, provider_config and present (names, flags, extra arguments and metadata are the same in every environment)".into());
            }
            return Ok(AgentCommand::EnvUpdate {
                entity: self.entity,
                select: self.select.map(Into::into),
                environment,
                config: self.config,
                provider_config: self.provider_config,
                present: self.present,
            });
        }
        if self.present.is_some() {
            return Err("`present` is per environment: give `environment` too".into());
        }
        let meta = super::EntityMeta {
            classification: self.classification,
            description: self.description,
            owner: self.owner,
        };
        match (self.entity, self.select) {
            (Some(entity), None) => Ok(AgentCommand::EntityUpdate {
                entity,
                name: self.name,
                config: self.config,
                provider_config: self.provider_config,
                manual: self.manual,
                providers: self.providers,
                extra: self.extra,
                extra_provider: self.extra_provider,
                extra_block: self.extra_block,
                meta,
                verbose: self.verbose,
            }),
            (None, Some(select)) => {
                if self.name.is_some() {
                    return Err("a bulk entity_update cannot rename: names must stay unique; rename one entity at a time".into());
                }
                Ok(AgentCommand::BulkUpdate {
                    select: select.into(),
                    changes: super::EntityChanges {
                        config: self.config,
                        provider_config: self.provider_config,
                        manual: self.manual,
                        providers: self.providers,
                        extra: self.extra,
                        extra_provider: self.extra_provider,
                        extra_block: self.extra_block,
                        meta,
                    },
                })
            }
            (Some(_), Some(_)) => Err("give `entity` or `select`, not both".into()),
            (None, None) => Err("give `entity` (one entity) or `select` (many)".into()),
        }
    }
}

/// Which entities a bulk `entity_update` / `link_add` applies to.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct SelectArgs {
    #[schemars(description = "Abstract type ids, e.g. [\"object_storage\", \"event_queue\"]")]
    pub types: Option<Vec<String>>,
    #[schemars(
        description = "Case-insensitive glob on the display name (`*` any run, `?` one character): \"jobs*\""
    )]
    pub name_glob: Option<String>,
    #[schemars(description = "Entity ids or display names")]
    pub ids: Option<Vec<String>>,
}

impl From<SelectArgs> for super::Selector {
    fn from(s: SelectArgs) -> Self {
        super::Selector {
            types: s.types.unwrap_or_default(),
            name_glob: s.name_glob,
            ids: s.ids.unwrap_or_default(),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SchemaSearchArgs {
    #[schemars(description = "Provider id; defaults to the target provider")]
    pub provider: Option<String>,
    #[schemars(description = "Space-separated terms matched against resource type names, e.g. `sqs queue`")]
    pub query: String,
    #[schemars(
        description = "`resource` or `data` to search only resources or only data sources; omit for both (each hit says which it is)"
    )]
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SchemaShowArgs {
    pub provider: Option<String>,
    #[schemars(
        description = "Resource type, e.g. `aws_s3_bucket_policy`, or a data source as `data.aws_ec2_managed_prefix_list`"
    )]
    pub resource: String,
    #[schemars(description = "`data` to read `resource` as a data source name without the `data.` prefix")]
    pub kind: Option<String>,
    #[schemars(
        description = "Levels of nested blocks to include (0 = attributes only); omit for everything. Some resources (e.g. aws_wafv2_web_acl) run to hundreds of KB unfiltered"
    )]
    pub depth: Option<u32>,
    #[schemars(description = "Only required attributes and nested blocks")]
    pub required_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    #[schemars(
        description = "Entity id or name, or — in the view — a note title, logical node name or group label"
    )]
    pub entity: String,
    pub x: i32,
    pub y: i32,
    #[schemars(
        description = "Move it in this view's own layout; defaults to the active view (the shared layout when that is All)"
    )]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ResizeArgs {
    #[schemars(
        description = "Entity id or name, or — in the view — a note title, logical node name or group label"
    )]
    pub entity: String,
    pub w: i32,
    pub h: i32,
    #[schemars(description = "View the annotation lives in; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SetParentArgs {
    pub entity: String,
    #[schemars(description = "Container id or name; omit for top level")]
    pub parent: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntitiesArgs {
    #[schemars(description = "Entity ids or names")]
    pub entities: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LinkAddArgs {
    #[schemars(description = "Source entity (id or name). Give this, or `select` to link many at once")]
    pub source: Option<String>,
    #[schemars(
        description = "Bulk link: link every entity this matches to `target`, as ONE undo step (\"put all 24 resources under one key\"). The target itself, and entities whose container already implies the link, are skipped and named; if any match's type may not have `relation` to the target's type the whole call changes nothing and every refusal is listed. An empty or non-matching selection is refused"
    )]
    pub select: Option<SelectArgs>,
    pub target: String,
    #[schemars(
        description = "Relation key: network_membership, attribute_reference, iam_binding, attachment, sends_to, reads, logs_to, encrypted_with, dead_letters_to, calls, depends_on. Must be allowed by the source type's definition for the target type (catalog_relations lists them)"
    )]
    pub relation: String,
    #[schemars(description = "Provider layers this link belongs to; omit for every provider")]
    pub providers: Option<Vec<String>>,
    #[schemars(
        description = "false: reply with only status, id and changed instead of the whole entity (settings_set { verbose } sets the default for the session)"
    )]
    pub verbose: Option<bool>,
    #[schemars(
        description = "Named environments this link belongs to (e.g. [\"prod\"]); [] = every environment. Works on an existing link too"
    )]
    pub environments: Option<Vec<String>>,
}

impl LinkAddArgs {
    /// The single-link or the bulk command these arguments ask for.
    fn into_command(self) -> Result<AgentCommand, String> {
        match (self.source, self.select) {
            (Some(source), None) => {
                let add = AgentCommand::LinkAdd {
                    source: source.clone(),
                    target: self.target.clone(),
                    relation: self.relation.clone(),
                    providers: self.providers,
                    verbose: self.verbose,
                };
                Ok(match self.environments {
                    None => add,
                    Some(environments) => AgentCommand::Batch(vec![
                        add,
                        AgentCommand::LinkEnvironments {
                            source,
                            target: self.target,
                            relation: self.relation,
                            environments,
                        },
                    ]),
                })
            }
            (None, Some(_)) if self.environments.is_some() => {
                Err("environment tags are set one link at a time: give `source`".into())
            }
            (None, Some(select)) => Ok(AgentCommand::BulkLink {
                select: select.into(),
                target: self.target,
                relation: self.relation,
                providers: self.providers,
            }),
            (Some(_), Some(_)) => Err("give `source` or `select`, not both".into()),
            (None, None) => Err("give `source` (one entity) or `select` (many)".into()),
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LinkRemoveArgs {
    pub source: String,
    pub target: String,
    #[schemars(description = "Relation key; omit to remove every link between the two")]
    pub relation: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewSetArgs {
    #[schemars(
        description = "Filter object: { categories: [..], relations: [..], focus: entity, depth: n, hidden: [..], only: [..], containers: bool, providers: [\"aws\"], origin: \"all\"|\"curated\"|\"native\", types: [\"subnet\"], name_glob: \"jobs*\" (case-insensitive, `*` and `?`), hide_edges: bool, spelt `hide_links` too (draw no structural links, for a pure data-flow view) }. Empty object shows everything"
    )]
    pub filter: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewSaveArgs {
    pub name: String,
    #[schemars(
        description = "Overwrite the filter of the view that already has this name, keeping its layout, groups, flows, notes and logical nodes. Without it a name already in use is refused"
    )]
    pub replace: Option<bool>,
}

/// The view a drawing command lands in; the active one when omitted.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewNameArg {
    #[schemars(description = "View name; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewGetArgs {
    #[schemars(description = "View name; defaults to the active view")]
    pub name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewUpdateArgs {
    #[schemars(description = "View to change; defaults to the active view")]
    pub view: Option<String>,
    #[schemars(description = "New name")]
    pub name: Option<String>,
    #[schemars(description = "What this view is for; shown under the view bar and in view_export")]
    pub description: Option<String>,
    #[schemars(description = "Show the legend panel on this view")]
    pub legend: Option<bool>,
    #[schemars(
        description = "Replace the view's saved filter (same object as view_set, names allowed in focus/hidden/only). The canvas follows when this view is active"
    )]
    pub filter: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewExportArgs {
    pub view: Option<String>,
    #[schemars(
        description = "`md` (default), `mermaid` (flowchart) or `sequence` (Mermaid sequenceDiagram of the numbered flows)"
    )]
    pub format: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GroupAddArgs {
    pub label: String,
    #[schemars(description = "Top-left canvas position and size of the box")]
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[schemars(description = "`#rrggbb`; a palette colour when omitted")]
    pub color: Option<String>,
    #[schemars(description = "View to draw in; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FlowAddArgs {
    #[schemars(
        description = "Resource (id or name), group (id or label) or logical node (id or name) the data comes from"
    )]
    pub from: String,
    #[schemars(description = "Resource, group or logical node the data goes to")]
    pub to: String,
    #[schemars(description = "Arrow label, e.g. `job requests`")]
    pub label: Option<String>,
    #[schemars(description = "Draw it dashed: optional or asynchronous")]
    pub dashed: Option<bool>,
    #[schemars(description = "Position in the sequence, drawn as a badge where the arrow starts")]
    pub step: Option<u32>,
    #[schemars(description = "`#rrggbb` or a group palette colour; the default ink when omitted")]
    pub color: Option<String>,
    #[schemars(
        description = "What travels along it, e.g. `call audio`: drawn under the label and listed in the exports"
    )]
    pub data: Option<String>,
    #[schemars(description = "View to draw in; defaults to the active view")]
    pub view: Option<String>,
    #[schemars(
        description = "When an end is hidden by the view's filter the flow is still added, with a `warning` naming the hidden end(s). With true the hidden resource is first taken out of the filter's `hidden` list (and added to `only` when that is non-empty), in the same undo step. Parts of a filter that define the view (categories, types, origin, providers, name_glob, focus) are never rewritten; the warning says when one of them is the reason"
    )]
    pub show_hidden: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NoteAddArgs {
    pub title: String,
    #[schemars(description = "Body text; wrapped when drawn")]
    pub body: Option<String>,
    #[schemars(
        description = "Resource, group, logical node or flow (id, name or label) this note explains: the note is drawn beside it and moves with it"
    )]
    pub anchor: Option<String>,
    #[schemars(description = "Top-left canvas position; defaults to the right of the diagram")]
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: Option<i32>,
    pub h: Option<i32>,
    #[schemars(description = "View to draw in; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LogicalAddArgs {
    #[schemars(description = "Display name, e.g. `users' browser`")]
    pub name: String,
    #[schemars(description = "Short text for the icon block, e.g. `WEB`")]
    pub icon: Option<String>,
    #[schemars(description = "One line under the name")]
    pub subtitle: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: Option<i32>,
    pub h: Option<i32>,
    #[schemars(description = "View to draw in; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    #[schemars(description = "Id, label, title or name of the group, flow, note or logical node")]
    pub key: String,
    #[schemars(description = "View to remove it from; defaults to the active view")]
    pub view: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ScreenshotArgs {
    #[schemars(description = "Activate this view before capturing")]
    pub view: Option<String>,
    #[schemars(description = "Zoom to fit the view's content first")]
    pub fit: Option<bool>,
    #[schemars(
        description = "Hide the palette, inspector and agent window for the frame so the canvas fills the image"
    )]
    pub hide_panels: Option<bool>,
    #[schemars(
        description = "Resize the window to this width in points for the capture, then put it back. 320-4096; the OS may clamp it to the display, so the reply reports the size actually captured. Use it when a fitted view is too small to read"
    )]
    pub width: Option<u32>,
    #[schemars(description = "Window height for the capture; see `width`")]
    pub height: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TidyArgs {
    #[schemars(
        description = "Only tidy the contents of this container (id or name); omit for the whole diagram"
    )]
    pub container: Option<String>,
    #[schemars(
        description = "Tidy this view, in its own layout (the shared layout never moves), then put its anchored notes back beside their anchors"
    )]
    pub view: Option<String>,
    #[schemars(
        description = "`links` (default): columns by dependency. `flows`: the view's data flows left to right in step order, crossings reduced, grouping boxes refitted around their members; needs a view"
    )]
    pub by: Option<String>,
}

/// Arguments of `view_generate`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewGenerateArgs {
    #[schemars(
        description = "View to draw the flows in (`data_flow`); defaults to the active view. Ignored for `personal_data`, which has a view of its own"
    )]
    pub view: Option<String>,
    #[schemars(
        description = "`data_flow`: flows between the view's visible resources derived from their data-carrying links (sends to, reads, uses, logs to, dead letters, calls, mounts, forwards to), in the direction the data moves. `personal_data`: build or refresh the view \"Where personal data goes\" from the resources classified personal or payment"
    )]
    pub kind: String,
    #[schemars(
        description = "data_flow only: first remove the view's resource-to-resource flows (flows from or to a group or logical node stay). Default false: pairs that already have a flow are left alone"
    )]
    pub replace: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AlignArgs {
    #[schemars(description = "left | hcenter | right | top | vcenter | bottom")]
    pub how: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DistributeArgs {
    pub horizontal: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SettingsArgs {
    #[schemars(description = "`terraform` or `opentofu`")]
    pub tool: Option<String>,
    #[schemars(description = "Target provider id")]
    pub provider: Option<String>,
    #[schemars(description = "Provider variables: { \"aws\": { \"region\": \"eu-west-2\" } }")]
    pub provider_settings: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(
        description = "Tags put on every generated resource (AWS default_tags, Google default_labels, an Azure `tags` argument): { \"Project\": \"CallScope\" }. Replaces the whole set; {} clears it"
    )]
    pub tags: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(
        description = "Also write Kubernetes manifests (k8s/: per workload a ServiceAccount, Deployment, Service, volumes, KEDA / HPA autoscaling, TargetGroupBinding) beside the Terraform on every export"
    )]
    pub kubernetes_manifests: Option<bool>,
    #[schemars(
        description = "Where the state lives; null for local state. {\"type\": \"s3\", \"bucket\": \"acme-tfstate\", \"region\": \"eu-west-2\"} (optional key_prefix or key, kms_key_id; locking by S3 lock file), {\"type\": \"azurerm\", \"resource_group_name\": …, \"storage_account_name\": …, \"container_name\": …} (optional key_prefix or key), {\"type\": \"gcs\", \"bucket\": …} (optional key_prefix), {\"type\": \"local\"} (optional path). The state key is <key_prefix>/terraform.tfstate, key_prefix defaulting to the project name. The export adds a bootstrap/ root that creates the bucket. Replaces the whole backend"
    )]
    #[serde(default, deserialize_with = "present")]
    pub backend: Option<serde_json::Value>,
    #[schemars(
        description = "Encrypt the state and plans (OpenTofu only; with Terraform it only produces a warning)"
    )]
    pub state_encryption: Option<bool>,
    #[schemars(
        description = "Id or name of the Encryption Key entity whose key encrypts the state (aws_kms on AWS, gcp_kms on Google Cloud; Azure uses a passphrase). The export moves that key to the bootstrap/ root. null clears it (passphrase encryption)"
    )]
    #[serde(default, deserialize_with = "present")]
    pub state_encryption_key: Option<serde_json::Value>,
    #[schemars(
        description = "Provider version constraints for required_providers: { \"aws\": \"~> 6.0\" }. Merged into the existing pins; null or \"\" removes a pin (back to the definition's default)"
    )]
    pub provider_versions: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(
        description = "Rename the project. The name heads the export's README and is the default state key prefix"
    )]
    pub name: Option<String>,
    #[schemars(
        description = "Session default for entity_add / entity_update / link_add replies: false returns only status, id and changed (a call's own verbose wins). Not saved with the project"
    )]
    pub verbose: Option<bool>,
    #[schemars(
        description = "Named environments, in order, e.g. [\"staging\", \"prod\"]: replaces the list; overrides and variable values of an environment no longer listed go with it (the reply names the entities that lost some). [] removes environments altogether. Each environment gets environments/<env>.tfvars and its own state key in the export"
    )]
    pub environments: Option<Vec<String>>,
    #[schemars(
        description = "Rename an environment, carrying its overrides: { \"from\": \"pilot\", \"to\": \"staging\" }"
    )]
    pub rename_environment: Option<RenameArg>,
    #[schemars(
        description = "Put in front of every generated resource name, e.g. \"zipos-${var.environment}\" (staging and prod can then share an account); null or \"\" clears it"
    )]
    #[serde(default, deserialize_with = "present")]
    pub name_prefix: Option<serde_json::Value>,
    #[schemars(
        description = "Project variables, merged: { \"db_class\": { \"value\": \"db.t4g.small\", \"environments\": { \"prod\": \"db.r6g.large\" }, \"description\": \"…\" } } (a bare value sets the base value; null removes one). Use them in any field as \"${var.db_class}\"; ${var.environment} and ${var.name_prefix} are built in"
    )]
    pub variables: Option<serde_json::Map<String, serde_json::Value>>,
    /// Anything else the caller sent: refused with the list of valid keys.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

/// `Some(value)` for a key that is present, `null` included, so `"backend": null` (clear
/// it) differs from leaving `backend` out (keep it).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    <serde_json::Value as serde::Deserialize>::deserialize(d).map(Some)
}

/// The keys `settings_set` takes, for the message that refuses any other.
pub const SETTINGS_KEYS: &[&str] = &[
    "tool",
    "provider",
    "provider_settings",
    "tags",
    "kubernetes_manifests",
    "backend",
    "state_encryption",
    "state_encryption_key",
    "provider_versions",
    "name",
    "verbose",
    "environments",
    "rename_environment",
    "name_prefix",
    "variables",
];

/// `{ "from": …, "to": … }` for `settings_set { rename_environment }`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RenameArg {
    pub from: String,
    pub to: String,
}

impl SettingsArgs {
    /// The command, or why the arguments are refused: a key `settings_set` does not
    /// know is an error rather than something silently ignored.
    pub fn into_command(self) -> Result<AgentCommand, String> {
        if !self.unknown.is_empty() {
            let names: Vec<String> = self.unknown.keys().map(|k| format!("`{k}`")).collect();
            return Err(format!(
                "settings_set does not take {}; valid keys: {}",
                names.join(", "),
                SETTINGS_KEYS.join(", ")
            ));
        }
        let env_keys = self.environments.is_some()
            || self.rename_environment.is_some()
            || self.name_prefix.is_some()
            || self.variables.is_some();
        let other_keys = self.tool.is_some()
            || self.provider.is_some()
            || self.provider_settings.is_some()
            || self.tags.is_some()
            || self.kubernetes_manifests.is_some()
            || self.backend.is_some()
            || self.state_encryption.is_some()
            || self.state_encryption_key.is_some()
            || self.provider_versions.is_some()
            || self.name.is_some()
            || self.verbose.is_some();
        let env = AgentCommand::EnvSettings {
            environments: self.environments,
            rename: self.rename_environment.map(|r| (r.from, r.to)),
            name_prefix: self.name_prefix,
            variables: self.variables,
        };
        let settings = AgentCommand::SettingsSet {
            tool: self.tool,
            provider: self.provider,
            provider_settings: self.provider_settings,
            tags: self.tags,
            kubernetes_manifests: self.kubernetes_manifests,
            backend: self.backend,
            state_encryption: self.state_encryption,
            state_encryption_key: self.state_encryption_key,
            provider_versions: self.provider_versions,
            name: self.name,
            verbose: self.verbose,
        };
        Ok(match (env_keys, other_keys) {
            (true, true) => AgentCommand::Batch(vec![settings, env]),
            (true, false) => env,
            _ => settings,
        })
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    #[schemars(description = "File path; omit to save to the current file")]
    pub path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenArgs {
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NewArgs {
    pub name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PlanArgs {
    #[schemars(description = "Provider id; defaults to the target provider")]
    pub provider: Option<String>,
    #[schemars(
        description = "Named environment to plan (its environments/<env>.tfvars); defaults to the first"
    )]
    pub environment: Option<String>,
    #[schemars(
        description = "Export directory; defaults to a folder of its own under the system's temporary directory"
    )]
    pub dir: Option<String>,
    #[schemars(description = "`terraform` or `opentofu`; defaults to the project's tool")]
    pub tool: Option<String>,
    #[schemars(
        description = "Plan against the configured backend (init -reconfigure -backend-config=environments/<env>.backend.hcl) instead of a scratch copy with local state"
    )]
    pub real_backend: Option<bool>,
    #[schemars(
        description = "Values for variables the files leave unset: { \"db_password\": \"…\" } (passed as -var)"
    )]
    pub vars: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExportArgs {
    #[schemars(description = "Directory to write the project into")]
    pub dir: String,
    pub provider: Option<String>,
    #[schemars(description = "Run `<tool> init && validate` afterwards (20-60 s)")]
    pub validate: Option<bool>,
    #[schemars(
        description = "Also write Kubernetes manifests into <dir>/k8s/ (with render.sh / render.ps1 and the k8s_* outputs they read), whatever settings.kubernetes_manifests says; false leaves them out"
    )]
    pub k8s: Option<bool>,
}

// ------------------------------------------------------------------ tools

/// How long the UI may go without draining before a command is refused instead of
/// queued, and how long we then give it to prove it is merely idle.
const STALE: Duration = Duration::from_secs(3);
const WAKE: Duration = Duration::from_millis(600);

/// The longest a call waits for the UI. Shorter than the 60 s many MCP clients allow, so
/// the caller hears from the server, with what happened, rather than from its own timer.
const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a write that needs approval waits for a quick Allow / Deny before answering
/// with its ticket.
const APPROVAL_GRACE: Duration = Duration::from_secs(3);

impl TtgServer {
    pub fn new(
        tx: mpsc::Sender<(AgentCommand, AgentReply)>,
        ctx: egui::Context,
        subs: Subscribers,
        beat: Arc<Heartbeat>,
        catalog_hash: String,
        approvals: Arc<Approvals>,
    ) -> Self {
        TtgServer {
            tx,
            ctx,
            subs,
            beat,
            session: SESSIONS.fetch_add(1, Ordering::Relaxed),
            catalog_hash,
            approvals,
            tool_router: Self::tool_router(),
        }
    }

    /// Refuse up front when the UI thread is stuck inside something long, so the
    /// command is never queued behind it: a queued command outlives the call that
    /// asked for it, and the retry that follows applies the work a second time.
    ///
    /// An idle app has not drained either — it only wakes on `request_repaint` — so a
    /// stale heartbeat first buys it [`WAKE`] to answer before we believe it.
    async fn refuse_if_busy(&self) -> Result<(), String> {
        let Some((_, busy)) = self.beat.stalled(STALE) else {
            return Ok(());
        };
        if busy.is_none() {
            self.ctx.request_repaint();
            let deadline = tokio::time::Instant::now() + WAKE;
            while tokio::time::Instant::now() < deadline {
                if self.beat.stalled(STALE).is_none() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let (since, busy) = self.beat.since_drain();
        Err(format!(
            "the app is busy{}; nothing was queued, retry in a moment (it last answered {:.0}s ago)",
            busy.map(|b| format!(": {b}")).unwrap_or_default(),
            since.as_secs_f32()
        ))
    }

    /// Queue a command for the UI thread and wait for its answer, never longer than
    /// [`CALL_TIMEOUT`]: the reply always arrives before a typical client gives up, and it
    /// always says whether anything was applied.
    ///
    /// A write that needs the user's approval comes back at once as a ticket
    /// (`pending_approval`); if the user answers within [`APPROVAL_GRACE`] the reply is
    /// the outcome instead. On a timeout the call is withdrawn, so the UI will never run
    /// it; if the UI had already started it, the answer is waited for.
    async fn exec(&self, cmd: AgentCommand) -> Result<serde_json::Value, String> {
        self.refuse_if_busy().await?;
        let (reply, mut wait) = AgentReply::channel();
        if self.tx.send((cmd, reply)).is_err() {
            return Err("the app is shutting down; nothing was applied".into());
        }
        self.ctx.request_repaint();
        let answer = match tokio::time::timeout(CALL_TIMEOUT, &mut wait.rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => return Err("the app dropped the request without running it; nothing was applied".into()),
            Err(_) if wait.withdraw() => {
                return Err(format!(
                    "the app did not get to this call within {}s{}; it has been withdrawn and will not run later, so nothing was applied and it is safe to retry",
                    CALL_TIMEOUT.as_secs(),
                    self.beat
                        .since_drain()
                        .1
                        .map(|b| format!(" (it is busy: {b})"))
                        .unwrap_or_default()
                ))
            }
            // The UI claimed it just before the deadline: it is running, so wait.
            Err(_) => match wait.rx.await {
                Ok(r) => r,
                Err(_) => return Err("the app stopped while running this call; check project_changes".into()),
            },
        };
        match answer {
            Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("pending_approval") => {
                let ticket = v["ticket"].as_str().unwrap_or_default().to_string();
                Ok(self.await_ticket(&ticket, APPROVAL_GRACE).await.unwrap_or(v))
            }
            other => other,
        }
    }

    /// The ticket's status once it is no longer pending, if that happens within `grace`.
    async fn await_ticket(&self, ticket: &str, grace: Duration) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let st = self.approvals.status_json(ticket)?;
            if st["status"] != "pending_approval" {
                return Some(st);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn run(&self, cmd: AgentCommand) -> CallToolResult {
        match self.exec(cmd).await {
            Ok(v) => ok_json(v),
            Err(e) => fail(e),
        }
    }

    /// What an export of `provider` attributes to which entity; empty when the project
    /// cannot be generated (the tool's own errors then come back unattributed).
    async fn attribution(&self, provider: Option<String>) -> ttg_codegen::plan_run::Attribution {
        self.exec(AgentCommand::Attribution { provider })
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// `plan_run`'s second half, once the export is on disk: init and plan off the UI
    /// thread and the dispatch loop, then the badges on the canvas.
    async fn plan_after_export(
        &self,
        prep: serde_json::Value,
        real_backend: bool,
        vars: std::collections::BTreeMap<String, String>,
    ) -> Result<serde_json::Value, String> {
        let attribution: ttg_codegen::plan_run::Attribution =
            serde_json::from_value(prep["attribution"].clone()).map_err(|e| e.to_string())?;
        let tool: ttg_core::Tool = serde_json::from_value(prep["tool"].clone()).map_err(|e| e.to_string())?;
        let dir = prep["dir"].as_str().ok_or("no export directory")?.to_string();
        let opts = ttg_codegen::plan_run::PlanOptions {
            var_file: prep["var_file"].as_str().map(str::to_string),
            backend_config: prep["backend_config"].as_str().map(str::to_string),
            real_backend,
            vars,
        };
        let report = tokio::task::spawn_blocking(move || {
            ttg_codegen::plan_run::run(std::path::Path::new(&dir), tool, &attribution, &opts)
        })
        .await
        .map_err(|e| format!("plan task failed: {e}"))?;
        let json = serde_json::to_value(&report).unwrap_or_default();
        let _ = self.exec(AgentCommand::PlanShow { report: json.clone() }).await;
        let mut out = json;
        out["environment"] = prep["environment"].clone();
        Ok(out)
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ApplyItem {
    #[schemars(description = "Name of a write tool, e.g. `entity_add`")]
    pub tool: String,
    #[schemars(description = "That tool's arguments")]
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ApplyArgs {
    pub commands: Vec<ApplyItem>,
    #[schemars(
        description = "Play the batch and report what it would do, then take it all back: per-command results plus the diagnostics that would appear and disappear (`diagnostics.added` / `removed`). Nothing is kept: not in the project, not in the undo history, not in the revision counter"
    )]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ProjectGetArgs {
    #[schemars(
        description = "Top-level keys to return, e.g. [\"nodes\", \"edges\"]. One of schema_version, name, settings, containers, nodes, edges, views. Omit for everything (about 60 KB for a large project)"
    )]
    pub fields: Option<Vec<String>>,
    #[schemars(
        description = "Entity ids or names: return only these entities and the links between them (containers, nodes, edges unless `fields` says otherwise)"
    )]
    pub entities: Option<Vec<String>>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct DiagnosticsArgs {
    #[schemars(description = "Only diagnostics about this entity (id or name)")]
    pub entity: Option<String>,
    #[schemars(description = "Only this severity: `error`, `warning` or `info`")]
    pub severity: Option<String>,
    #[schemars(
        description = "Provider whose run to report (`aws` / `azure` / `gcp`); defaults to the project's target provider. Nothing about the project changes"
    )]
    pub provider: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct CatalogRelationsArgs {
    #[schemars(description = "Abstract type the relations start from, e.g. `object_storage`")]
    pub source_type: Option<String>,
    #[schemars(description = "Only relations whose targets include this type, e.g. `encryption_key`")]
    pub target_type: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntityPreviewArgs {
    #[schemars(description = "Entity id or display name")]
    pub entity: String,
    #[schemars(description = "Provider id; defaults to the project's target provider")]
    pub provider: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct CostArgs {
    #[schemars(description = "Provider to price (`aws` / `azure` / `gcp`); defaults to the target provider")]
    pub provider: Option<String>,
    #[schemars(
        description = "Named environment to price (settings.environments): its overrides, absent entities and variable values apply. Defaults to the environment the canvas shows, else the base values"
    )]
    pub environment: Option<String>,
    #[schemars(
        description = "Only the entities this saved view shows (name, case-insensitive); its groups get totals"
    )]
    pub view: Option<String>,
    #[schemars(
        description = "`entity` (default: one line per priced entity, largest first), `type` (totals per abstract type) or `group` (totals per labelled box of `view`, or of every view)"
    )]
    pub group_by: Option<String>,
    #[schemars(
        description = "Assumption values for this call only, e.g. { \"pool_node_hours_per_day\": 4 }. Nothing is saved; the reply lists every assumption with its value and source"
    )]
    pub assumptions: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(description = "Price for this region instead of the project's, e.g. `us-east-1`")]
    pub region: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiffArgs {
    #[schemars(description = "Directory a previous export wrote to")]
    pub dir: String,
    pub provider: Option<String>,
    #[schemars(description = "Compare an export with (true) or without (false) Kubernetes manifests")]
    pub k8s: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ApprovalStatusArgs {
    #[schemars(
        description = "Ticket from a reply whose status was pending_approval. Omit to list every ticket still known (pending ones and those answered in the last 30 minutes)"
    )]
    pub ticket: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImportArgs {
    #[schemars(
        description = "The project: a JSON object in the .ttg.json format (schemas/project.schema.json), or that JSON as a string. At least schema_version and name; containers, nodes, edges, views and settings as in a saved file"
    )]
    pub json: serde_json::Value,
    #[schemars(
        description = "Replace the open project when it already has entities. Without it the import is refused unless the open project is empty. The replaced project comes back with one undo; when it has unsaved changes the user is asked first"
    )]
    pub replace: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ChangesArgs {
    #[schemars(description = "Revision returned by an earlier project_changes / project_summary call")]
    pub since: Option<u64>,
}

#[tool_router]
impl TtgServer {
    #[tool(
        description = "The whole project as JSON (same shape as the .ttg.json file): settings, containers, nodes, edges, views. That is large (about 60 KB for a real design): pass `fields` (e.g. [\"nodes\", \"edges\"]) and/or `entities` (ids or names: just those entities and the links between them) to get a slice. Anchored notes in `views` store their `position` as an offset from the anchor, as the file does; view_get reports the absolute position and the offset separately."
    )]
    async fn project_get(&self, Parameters(a): Parameters<ProjectGetArgs>) -> CallToolResult {
        if a.fields.is_none() && a.entities.is_none() {
            return self.run(AgentCommand::ProjectGet).await;
        }
        self.run(AgentCommand::ProjectSlice {
            fields: a.fields,
            entities: a.entities,
        })
        .await
    }

    #[tool(
        description = "Counts, tool, target provider, file path, dirty flag, diagnostics totals, saved views, current selection."
    )]
    async fn project_summary(&self) -> CallToolResult {
        self.run(AgentCommand::ProjectSummary).await
    }

    #[tool(
        description = "Every abstract resource type in the catalog with category, kind, provider mapping status and allowed parents."
    )]
    async fn catalog_types(&self) -> CallToolResult {
        self.run(AgentCommand::CatalogTypes).await
    }

    #[tool(
        description = "Full definition of one type: fields (with types, defaults, options), relations it may have, per-provider fields and generated resources."
    )]
    async fn catalog_type(&self, Parameters(a): Parameters<TypeArgs>) -> CallToolResult {
        self.run(AgentCommand::CatalogType { type_id: a.type_id }).await
    }

    #[tool(
        description = "Search the provider schema for resource types and data sources (1,700+ AWS / 1,100+ Azure / 1,300+ Google Cloud resources, and their data sources). Each hit has a `kind` (resource | data) and a `type_id`: add it with entity_add (`native:<provider>:<resource>`, or `native:<provider>:data.<data source>` for a lookup of something that already exists, e.g. an AWS-managed prefix list); then set its arguments with entity_update.extra and reference it from other entities with {\"$ref\": {\"entity\": …, \"attr\": …}}."
    )]
    async fn schema_search(&self, Parameters(a): Parameters<SchemaSearchArgs>) -> CallToolResult {
        self.run(AgentCommand::SchemaSearch {
            provider: a.provider,
            query: a.query,
            kind: a.kind,
        })
        .await
    }

    #[tool(
        description = "Every argument and nested block of a provider resource type, with types, required flags and descriptions. Large resources can run to hundreds of KB unfiltered; narrow with `depth` and/or `required_only`."
    )]
    async fn schema_show(&self, Parameters(a): Parameters<SchemaShowArgs>) -> CallToolResult {
        self.run(AgentCommand::SchemaShow {
            provider: a.provider,
            resource: a.resource,
            kind: a.kind,
            depth: a.depth,
            required_only: a.required_only,
        })
        .await
    }

    #[tool(
        description = "Current diagnostics (errors block export; warnings become manual steps) for the target provider, under `diagnostics`. `other_providers` carries what the *other* providers would say: their errors as warnings (they do not block this export, they say what switching the target would cost) and, as info, each entity one of them leaves out of its export. Filter with `entity`, `severity` (error | warning | info) and `provider` (whose run to report; defaults to the target); the filters apply to both lists and the reply says how many matched of how many."
    )]
    async fn diagnostics(&self, Parameters(a): Parameters<DiagnosticsArgs>) -> CallToolResult {
        if a.entity.is_none() && a.severity.is_none() && a.provider.is_none() {
            return self.run(AgentCommand::Diagnostics).await;
        }
        self.run(AgentCommand::DiagnosticsFiltered {
            entity: a.entity,
            severity: a.severity,
            provider: a.provider,
        })
        .await
    }

    #[tool(
        description = "Network posture of every resource: subnets, security groups (a workload has its cluster's or pool's; '(cluster security group)' is the group the provider makes for a cluster), way out of the network, internet exposure, listening port."
    )]
    async fn reach_posture(&self) -> CallToolResult {
        self.run(AgentCommand::ReachPosture).await
    }

    #[tool(description = "What one resource can reach, with the path and the reason when blocked.")]
    async fn reach_from(&self, Parameters(a): Parameters<EntityArgs>) -> CallToolResult {
        self.run(AgentCommand::ReachFrom { entity: a.entity }).await
    }

    #[tool(description = "Who can reach one (passive) resource, with the path and the reason when blocked.")]
    async fn reach_to(&self, Parameters(a): Parameters<EntityArgs>) -> CallToolResult {
        self.run(AgentCommand::ReachTo { entity: a.entity }).await
    }

    #[tool(
        description = "Generate the Terraform/OpenTofu files in memory and return them as text, without writing to disk. Fails with the blocking diagnostics if there are errors (export_diff, export_run and entity_preview fail with the same list). For one entity's blocks only, use entity_preview."
    )]
    async fn export_preview(&self, Parameters(a): Parameters<PreviewArgs>) -> CallToolResult {
        self.run(AgentCommand::ExportPreview {
            provider: a.provider,
            k8s: a.k8s,
        })
        .await
    }

    #[tool(
        description = "The HCL one entity produces for a provider: just its blocks, rendered exactly as the export renders them, with the resource addresses, the file they land in, the entity's manual steps and its diagnostics. An entity that produces nothing (external, logical on that provider, tagged for another, left out by a check) comes back with empty `hcl` and `no_blocks` saying why. Fails with the blocking diagnostics, like export_preview, while the project has errors."
    )]
    async fn entity_preview(&self, Parameters(a): Parameters<EntityPreviewArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityPreview {
            entity: a.entity,
            provider: a.provider,
        })
        .await
    }

    #[tool(
        description = "Estimated monthly cost of the provider's layer, from bundled list prices (USD, dated; on-demand, no free tier or discounts) and editable usage assumptions (GB stored, node-hours a day of a pool that scales to zero, requests...). Per entity: the charges (quantity x unit price), the assumptions used and why anything is free or not estimated; totals per type, per saved view and per view group; the price date and a caveat to repeat to the user. Never writes anything."
    )]
    async fn cost_estimate(&self, Parameters(a): Parameters<CostArgs>) -> CallToolResult {
        self.run(AgentCommand::CostEstimate {
            provider: a.provider,
            environment: a.environment,
            view: a.view,
            group_by: a.group_by,
            assumptions: a.assumptions,
            region: a.region,
        })
        .await
    }

    #[tool(
        description = "Which relations the definitions let one type have to another: relation key (what link_add takes), label, target types, cardinality, whether containment satisfies it (`via_parent`), and the providers it applies to (empty = all). Give `source_type` and/or `target_type` to narrow it, e.g. source object_storage, target encryption_key. `depends_on` is always allowed between any two entities."
    )]
    async fn catalog_relations(&self, Parameters(a): Parameters<CatalogRelationsArgs>) -> CallToolResult {
        self.run(AgentCommand::CatalogRelations {
            source_type: a.source_type,
            target_type: a.target_type,
        })
        .await
    }

    #[tool(
        description = "A PNG screenshot of the app window (base64 in `data`). With `view` it switches to that view first, `fit` frames its content, and `hide_panels` drops the side panels so the canvas fills the image. `width`/`height` resize the window for the capture and put it back afterwards (the fit happens at the new size, so a big view stays readable); the OS may clamp the request to the display, so the reply's metadata reports the size actually captured alongside the one asked for. Use it to look at what you have drawn."
    )]
    async fn screenshot(&self, Parameters(a): Parameters<ScreenshotArgs>) -> CallToolResult {
        if let Err(e) = self.refuse_if_busy().await {
            return fail(e);
        }
        let (reply_tx, wait) = AgentReply::channel();
        let resized = a.width.is_some() || a.height.is_some();
        let cmd = AgentCommand::Screenshot {
            fit: a.fit.unwrap_or(false),
            view: a.view,
            hide_panels: a.hide_panels.unwrap_or(false),
            width: a.width,
            height: a.height,
        };
        if self.tx.send((cmd, reply_tx)).is_err() {
            return fail("the app is shutting down");
        }
        self.ctx.request_repaint();
        // A resize needs a few more frames (and a window manager round trip).
        let wait_s = if resized { 30 } else { 15 };
        match tokio::time::timeout(Duration::from_secs(wait_s), wait.rx).await {
            Ok(Ok(Ok(v))) => {
                let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("").to_string();
                let meta = serde_json::json!({
                    "width": v["width"],
                    "height": v["height"],
                    "asked_for": v["asked_for"],
                });
                CallToolResult::success(vec![
                    Content::image(data, "image/png"),
                    Content::text(meta.to_string()),
                ])
            }
            Ok(Ok(Err(e))) => fail(e),
            _ => fail("no screenshot arrived (the window may be minimised)"),
        }
    }

    #[tool(
        description = "Add a resource to the canvas. Returns its id. The user sees it appear immediately; every write is one undo step."
    )]
    async fn entity_add(&self, Parameters(a): Parameters<EntityAddArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityAdd {
            type_id: a.type_id,
            name: a.name,
            parent: a.parent,
            x: a.x,
            y: a.y,
            providers: a.providers,
            verbose: a.verbose,
        })
        .await
    }

    #[tool(
        description = "Rename, set abstract/provider field values (checked against the definition), the external flag, or what the resource is for: `classification` (public | internal | confidential | personal | payment), `description` and `owner` (emitted as tags / labels and an HCL comment). Pass `entity` for one, or `select` ({ types, name_glob, ids }, every criterion given must match) to apply the same values to every match as ONE undo step; a bulk update is all-or-nothing, cannot rename, refuses an empty or non-matching selection, and lists what changed."
    )]
    async fn entity_update(&self, Parameters(a): Parameters<EntityUpdateArgs>) -> CallToolResult {
        match a.into_command() {
            Ok(cmd) => self.run(cmd).await,
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Move an entity to a canvas position (containers move with their contents). Lands in the view's own layout when one is active; pass `view` to arrange a named view without switching to it. It also moves a view's annotations — a note, a logical node or a grouping box, addressed by id, title, name or label — to that position; a group keeps its membership geometric, so moving the box does not drag what was inside it. The reply says which `kind` it moved."
    )]
    async fn entity_move(&self, Parameters(a): Parameters<MoveArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityMove {
            entity: a.entity,
            x: a.x,
            y: a.y,
            view: a.view,
        })
        .await
    }

    #[tool(
        description = "Resize a node or container, or one of a view's annotations (note, logical node, grouping box). The reply says which `kind` it resized."
    )]
    async fn entity_resize(&self, Parameters(a): Parameters<ResizeArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityResize {
            entity: a.entity,
            w: a.w,
            h: a.h,
            view: a.view,
        })
        .await
    }

    #[tool(
        description = "Put an entity inside a container (or at top level). Containment implies network membership for subnets etc."
    )]
    async fn entity_set_parent(&self, Parameters(a): Parameters<SetParentArgs>) -> CallToolResult {
        self.run(AgentCommand::EntitySetParent {
            entity: a.entity,
            parent: a.parent,
        })
        .await
    }

    #[tool(
        description = "Delete entities and their links. Children of a deleted container move up one level."
    )]
    async fn entity_delete(&self, Parameters(a): Parameters<EntitiesArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityDelete { entities: a.entities })
            .await
    }

    #[tool(
        description = "Link two entities. Direction: source depends on / uses target. Refused when the definition does not allow it or containment already implies it. Pass `select` ({ types, name_glob, ids }) instead of `source` to link every match to `target` in one undo step (all-or-nothing; entities already inside an implying container are skipped and named)."
    )]
    async fn link_add(&self, Parameters(a): Parameters<LinkAddArgs>) -> CallToolResult {
        match a.into_command() {
            Ok(cmd) => self.run(cmd).await,
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Remove a link.")]
    async fn link_remove(&self, Parameters(a): Parameters<LinkRemoveArgs>) -> CallToolResult {
        self.run(AgentCommand::LinkRemove {
            source: a.source,
            target: a.target,
            relation: a.relation,
        })
        .await
    }

    #[tool(
        description = "Select entities so the inspector and the reachability overlay follow along. Empty list clears the selection."
    )]
    async fn selection_set(&self, Parameters(a): Parameters<EntitiesArgs>) -> CallToolResult {
        self.run(AgentCommand::SelectionSet { entities: a.entities })
            .await
    }

    #[tool(
        description = "Apply a canvas filter (what is shown); does not affect what is generated. While a saved view is active this writes the filter into that view — the same thing the filter menu does in the app — so the view keeps showing what you last set; the reply names the view it changed. To filter without touching a saved view, switch to All (view_activate) first."
    )]
    async fn view_set(&self, Parameters(a): Parameters<ViewSetArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewSet { filter: a.filter }).await
    }

    #[tool(
        description = "Save the current filter as a named view tab in the project. New views own their layout: moves made while the view is active do not affect other views. A name that is already taken (case-insensitively) is refused unless `replace` is true, which rewrites that view's filter and leaves its layout, groups, flows, notes and logical nodes alone."
    )]
    async fn view_save(&self, Parameters(a): Parameters<ViewSaveArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewSave {
            name: a.name,
            replace: a.replace.unwrap_or(false),
        })
        .await
    }

    #[tool(
        description = "Delete a saved view and everything it holds (layout, groups, flows, notes, logical nodes) as one undo step. The canvas falls back to All when it was the active view."
    )]
    async fn view_delete(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewDelete { name: a.name }).await
    }

    #[tool(
        description = "Switch to a saved view by name (`All` for everything). Groups, flows and per-view positions apply to the active view."
    )]
    async fn view_activate(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewActivate { name: a.name }).await
    }

    #[tool(
        description = "Everything one view holds: description, filter, own layout, which resources it shows, and its groups (with their current members), flows, notes and logical nodes. Defaults to the active view. A note's `position` is where it is drawn (the absolute position entity_move takes); an anchored note also reports the `offset` from its anchor that it stores, null when it is free."
    )]
    async fn view_get(&self, Parameters(a): Parameters<ViewGetArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewGet { name: a.name }).await
    }

    #[tool(
        description = "Rename a view, set the description shown under the view bar (and at the top of its export), turn its legend panel on, or replace its saved filter. `filter` takes the same object as view_set and replaces the view's stored one outright; the canvas follows when that view is active."
    )]
    async fn view_update(&self, Parameters(a): Parameters<ViewUpdateArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewUpdate {
            view: a.view,
            name: a.name,
            description: a.description,
            legend: a.legend,
            filter: a.filter,
        })
        .await
    }

    #[tool(
        description = "Frame a view's content in the camera, as Zoom to fit does — switching to the view first when one is named. Returns the bounding box it fitted."
    )]
    async fn view_fit(&self, Parameters(a): Parameters<ViewNameArg>) -> CallToolResult {
        self.run(AgentCommand::ViewFit { view: a.view }).await
    }

    #[tool(
        description = "A view as a document: `md` gives the description, a table of the resources it shows (with classification, description and owner columns when any is set), the groups with their members, the flows in step order and the notes; `mermaid` gives a flowchart LR with the groups as subgraphs; `sequence` gives a Mermaid sequenceDiagram of the numbered flows (participants in order of appearance, shared step numbers as par blocks, dashed flows as -->>). For a picture, use screenshot with fit and hide_panels."
    )]
    async fn view_export(&self, Parameters(a): Parameters<ViewExportArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewExport {
            view: a.view,
            format: a.format.unwrap_or_else(|| "md".into()),
        })
        .await
    }

    #[tool(
        description = "Add a grouping box to a view: an architecture-map annotation, never exported. Membership is geometric: a resource, logical node or box whose CENTRE falls inside this one belongs to it, and for one box to nest inside another its centre must be inside and its area smaller (view_get reports that as `nested_in` / `parent`, and the Markdown export as the Inside column). Members move when you drag the box on the canvas."
    )]
    async fn view_group_add(&self, Parameters(a): Parameters<GroupAddArgs>) -> CallToolResult {
        self.run(AgentCommand::GroupAdd {
            view: a.view,
            label: a.label,
            x: a.x,
            y: a.y,
            w: a.w,
            h: a.h,
            color: a.color,
        })
        .await
    }

    #[tool(
        description = "Add a labelled data-flow arrow to a view between resources, groups and/or logical nodes, optionally numbered (`step`) and coloured. An annotation: not a dependency, never exported. If an end is hidden by the view's filter the flow is still added and the reply carries a `warning` naming it and how to show it (view_update { filter }); `show_hidden: true` shows it for you."
    )]
    async fn view_flow_add(&self, Parameters(a): Parameters<FlowAddArgs>) -> CallToolResult {
        self.run(AgentCommand::FlowAdd {
            view: a.view,
            from: a.from,
            to: a.to,
            label: a.label.unwrap_or_default(),
            dashed: a.dashed.unwrap_or(false),
            step: a.step,
            color: a.color,
            show_hidden: a.show_hidden.unwrap_or(false),
            data: a.data,
        })
        .await
    }

    #[tool(
        description = "Add a note box to a view: a title and a paragraph explaining part of the picture. Never exported. Anchor it to a resource, group, logical node or flow and it is drawn beside that thing and follows it."
    )]
    async fn view_note_add(&self, Parameters(a): Parameters<NoteAddArgs>) -> CallToolResult {
        self.run(AgentCommand::NoteAdd {
            view: a.view,
            title: a.title,
            body: a.body.unwrap_or_default(),
            x: a.x,
            y: a.y,
            w: a.w,
            h: a.h,
            anchor: a.anchor,
        })
        .await
    }

    #[tool(
        description = "Add an annotation-only node to a view — a browser, a telephony platform, one workload inside a cluster — so flows can start and end there instead of converging on one box. Nothing is generated for it."
    )]
    async fn view_logical_add(&self, Parameters(a): Parameters<LogicalAddArgs>) -> CallToolResult {
        self.run(AgentCommand::LogicalAdd {
            view: a.view,
            name: a.name,
            icon: a.icon,
            subtitle: a.subtitle,
            x: a.x,
            y: a.y,
            w: a.w,
            h: a.h,
        })
        .await
    }

    #[tool(
        description = "Remove a group, flow, note or logical node from a view by id, label, title or name."
    )]
    async fn view_annotation_remove(&self, Parameters(a): Parameters<KeyArgs>) -> CallToolResult {
        self.run(AgentCommand::AnnotationRemove {
            view: a.view,
            key: a.key,
        })
        .await
    }

    #[tool(
        description = "Auto-layout. `by: links` (default): columns by dependency, containers fitted to contents. `by: flows` with a `view`: that view's data flows left to right in step order, placed in the view's own layout only. With a `view`, its anchored notes are put back beside their anchors afterwards. Undoable."
    )]
    async fn layout_tidy(&self, Parameters(a): Parameters<TidyArgs>) -> CallToolResult {
        self.run(AgentCommand::LayoutTidy {
            container: a.container,
            view: a.view,
            by: a.by,
        })
        .await
    }

    // View tools: flows generated from links, notes put back beside their anchors.

    #[tool(
        description = "Generate view content from the model. `kind: data_flow` draws flows between the view's visible resources from their data-carrying links (structural links such as network membership, IAM bindings and encryption produce nothing), labelled by what the link does, never duplicating a pair that already has a flow; the reply lists what was added. `kind: personal_data` builds or refreshes the view \"Where personal data goes\": resources classified personal or payment, what their data reaches one link away, and the flows between them. One undo step."
    )]
    async fn view_generate(&self, Parameters(a): Parameters<ViewGenerateArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewGenerate {
            view: a.view,
            kind: a.kind,
            replace: a.replace.unwrap_or(false),
        })
        .await
    }

    #[tool(
        description = "Put every anchored note of a view back beside what it explains (the first clear side: right, below, left, above), e.g. after moving things. Free notes stay put. layout_tidy with a view does this itself."
    )]
    async fn view_arrange_notes(&self, Parameters(a): Parameters<ViewNameArg>) -> CallToolResult {
        self.run(AgentCommand::ViewArrangeNotes { view: a.view }).await
    }

    #[tool(description = "Align the current selection (2+ items).")]
    async fn layout_align(&self, Parameters(a): Parameters<AlignArgs>) -> CallToolResult {
        self.run(AgentCommand::LayoutAlign { how: a.how }).await
    }

    #[tool(description = "Distribute the current selection evenly (3+ items).")]
    async fn layout_distribute(&self, Parameters(a): Parameters<DistributeArgs>) -> CallToolResult {
        self.run(AgentCommand::LayoutDistribute {
            horizontal: a.horizontal,
        })
        .await
    }

    #[tool(
        description = "Change the tool, target provider, provider variables, project-wide default tags, whether Kubernetes manifests are exported, the state backend, state encryption (and the key it uses), provider version pins, or the named environments, the name prefix and project variables. Unknown keys are refused, listing the valid ones. project_get shows the result under `settings`; per-environment field values are set with entity_update { environment }."
    )]
    async fn settings_set(&self, Parameters(a): Parameters<SettingsArgs>) -> CallToolResult {
        match a.into_command() {
            Ok(cmd) => self.run(cmd).await,
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Save the project to disk. Never happens implicitly: ask the user before calling this."
    )]
    async fn project_save(&self, Parameters(a): Parameters<PathArg>) -> CallToolResult {
        self.run(AgentCommand::ProjectSave { path: a.path }).await
    }

    #[tool(
        description = "Open a project file. If the current project has unsaved changes the app shows its prompt and this returns 'prompt shown'."
    )]
    async fn project_open(&self, Parameters(a): Parameters<OpenArgs>) -> CallToolResult {
        self.run(AgentCommand::ProjectOpen { path: a.path }).await
    }

    #[tool(description = "Start a new empty project (same unsaved-changes prompt as the menu).")]
    async fn project_new(&self, Parameters(a): Parameters<NewArgs>) -> CallToolResult {
        self.run(AgentCommand::ProjectNew { name: a.name }).await
    }

    #[tool(
        description = "Write a complete Terraform/OpenTofu project directory for one provider, optionally validating it. With k8s (or settings.kubernetes_manifests) it also writes k8s/: Kubernetes manifests for the workloads plus render scripts that fill in the Terraform outputs they need."
    )]
    async fn export_run(&self, Parameters(a): Parameters<ExportArgs>) -> CallToolResult {
        let dir = a.dir.clone();
        let provider = a.provider.clone();
        let result = self
            .run(AgentCommand::ExportRun {
                dir: a.dir,
                provider: a.provider,
                k8s: a.k8s,
            })
            .await;
        if !a.validate.unwrap_or(false) || result.is_error.unwrap_or(false) {
            return result;
        }
        let reply = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t.text).ok())
            .unwrap_or_default();
        // What the files and lines belong to, so each validate error comes back on its entity.
        let attribution = self.attribution(provider).await;
        // The export reply itself, or the one an approved ticket carries.
        let export = match reply.get("status").and_then(|s| s.as_str()) {
            Some("pending_approval") => {
                // Nothing is written yet: validate once the user allows it, and put the
                // outcome on the ticket where approval_status reports it.
                let ticket = reply["ticket"].as_str().unwrap_or_default().to_string();
                let approvals = self.approvals.clone();
                tokio::spawn(
                    async move { validate_after_approval(approvals, ticket, dir, attribution).await },
                );
                let mut content = result.content;
                content.push(Content::text(
                    "validate: runs once the export is approved; approval_status { ticket } reports it as result.validate",
                ));
                return CallToolResult::success(content);
            }
            Some("applied") => reply["result"].clone(),
            Some("denied") | Some("expired") | Some("failed") => return result,
            _ => reply,
        };
        // Validate off the UI thread, each error attributed to its entity.
        let outcome = run_validate(dir, tool_of(&export), attribution).await;
        let mut content = result.content;
        let text =
            serde_json::to_string_pretty(&serde_json::json!({ "validate": outcome })).unwrap_or_default();
        content.push(Content::text(text));
        CallToolResult::success(content)
    }

    #[tool(
        description = "Export, then `init` and `plan` the result, and return the planned changes per entity: { status: planned | no_credentials | failed | binary_not_found, summary, create/update/delete/replace totals, entities: [{ entity, name, create, update, delete, replace, read, no_op, addresses: [{ address, actions, change }] }], diagnostics: [{ severity, summary, detail, file, line, entity, entity_name, definition }] }. A project with environments plans one of them (`environment`, default the first) with its environments/<env>.tfvars. Without `real_backend` it plans in a scratch copy with local state (what applying would create from nothing; the real state is never touched). A plan reads the cloud account, so it needs the provider's credentials; missing ones come back as status no_credentials. `vars` supplies variables the files leave unset (a database password). Runs in the background (a minute or more) and puts +, ~, −, ± badges on the canvas."
    )]
    async fn plan_run(&self, Parameters(a): Parameters<PlanArgs>) -> CallToolResult {
        let tool = match a.tool.as_deref() {
            None => None,
            Some("terraform") => Some(ttg_core::Tool::Terraform),
            Some("opentofu") | Some("tofu") => Some(ttg_core::Tool::OpenTofu),
            Some(other) => return fail(format!("tool must be terraform or opentofu, not \"{other}\"")),
        };
        let prep = match self
            .exec(AgentCommand::PlanPrepare {
                dir: a.dir,
                provider: a.provider,
                environment: a.environment,
                tool,
            })
            .await
        {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let real_backend = a.real_backend.unwrap_or(false);
        let vars: std::collections::BTreeMap<String, String> = a
            .vars
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or(v.to_string())))
            .collect();
        // The export may wait for the user's approval: then the plan runs once it is
        // allowed, and approval_status reports it as `result.plan`.
        if prep.get("status").and_then(|s| s.as_str()) == Some("pending_approval") {
            let ticket = prep["ticket"].as_str().unwrap_or_default().to_string();
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let Some(t) = me.approvals.get(&ticket) else {
                        return;
                    };
                    match t.outcome {
                        super::approvals::Outcome::Pending => {
                            tokio::time::sleep(Duration::from_millis(500)).await
                        }
                        super::approvals::Outcome::Applied(result) => {
                            me.approvals.amend(&ticket, "plan", serde_json::json!("running"));
                            let outcome = match me.plan_after_export(result, real_backend, vars).await {
                                Ok(v) => v,
                                Err(e) => serde_json::json!({ "status": "failed", "summary": e }),
                            };
                            me.approvals.amend(&ticket, "plan", outcome);
                            return;
                        }
                        _ => return,
                    }
                }
            });
            let mut out = prep;
            out["plan"] = serde_json::json!(
                "runs once the export is approved; approval_status { ticket } reports it as result.plan"
            );
            return ok_json(out);
        }
        match self.plan_after_export(prep, real_backend, vars).await {
            Ok(v) => ok_json(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Apply several diagram writes (entity_*, link_*, view_*, layout_*, selection_set, settings_set) as ONE undo step. Stops at the first failure and rolls the whole batch back. Returns each command's result. With `dry_run: true` the batch is played and then taken back: the reply has each command's result and the diagnostics it would add or remove, and nothing is kept (not in the project, the undo history or the revision counter)."
    )]
    async fn project_apply(&self, Parameters(a): Parameters<ApplyArgs>) -> CallToolResult {
        let mut cmds = Vec::with_capacity(a.commands.len());
        for (i, it) in a.commands.into_iter().enumerate() {
            let name = it.tool.clone();
            match command_from_json(&it.tool, it.args) {
                Ok(c) => cmds.push(c),
                Err(e) => return fail(format!("command {i} ({name}): {e}")),
            }
        }
        if cmds.is_empty() {
            return fail("no commands");
        }
        if a.dry_run.unwrap_or(false) {
            return self.run(AgentCommand::DryRun(cmds)).await;
        }
        self.run(AgentCommand::Batch(cmds)).await
    }

    #[tool(
        description = "What exporting would change in an existing export directory: per-file added/removed line counts and unified diffs. Nothing is written."
    )]
    async fn export_diff(&self, Parameters(a): Parameters<DiffArgs>) -> CallToolResult {
        self.run(AgentCommand::ExportDiff {
            dir: a.dir,
            provider: a.provider,
            k8s: a.k8s,
        })
        .await
    }

    #[tool(
        description = "Has the diagram changed (by the user or the agent) since a revision? Returns the current revision, who changed it last and how long ago. Poll this between edits, or subscribe to the ttg://project resource for push notifications."
    )]
    async fn project_changes(&self, Parameters(a): Parameters<ChangesArgs>) -> CallToolResult {
        self.run(AgentCommand::Changes { since: a.since }).await
    }

    // Approval tickets and file interchange.

    #[tool(
        description = "Outcome of a write that needed the user's approval. Such a write (save, open, new, export, import over unsaved work, delete when the user asks for that) answers within seconds with { status: \"pending_approval\", ticket, what, applied: false } instead of waiting for the prompt. Poll this with the ticket every few seconds: it answers at once with pending_approval (and how many prompts are ahead and when it expires), applied (applied: true, with the command's result), failed (allowed, but the command failed: nothing applied), denied, or expired (nobody answered in time: nothing applied). Never repeat the original call while its ticket is pending. Without a ticket it lists every ticket still known."
    )]
    async fn approval_status(&self, Parameters(a): Parameters<ApprovalStatusArgs>) -> CallToolResult {
        match a.ticket {
            None => ok_json(serde_json::json!({ "tickets": self.approvals.list_json() })),
            Some(t) => match self.approvals.status_json(&t) {
                Some(v) => ok_json(v),
                None => fail(format!(
                    "unknown ticket {t}: tickets are kept for 30 minutes after they are answered (and lost when the app restarts); approval_status without a ticket lists the known ones"
                )),
            },
        }
    }

    #[tool(
        description = "Replace the open project with one given as .ttg.json JSON (the format project_get returns and the app saves; schema: https://raw.githubusercontent.com/geawilliamsUK/TerraTofuGUi/master/schemas/project.schema.json). The JSON is checked against the schema first and every problem is reported with its line; nothing changes unless it loads. Refused while the open project has entities unless `replace` is true; the replaced project comes back with one undo. When the open project has unsaved changes the user is asked first (the reply is then an approval ticket). The imported project is not saved: it has no file until project_save { path }."
    )]
    async fn project_import(&self, Parameters(a): Parameters<ImportArgs>) -> CallToolResult {
        let text = match a.json {
            serde_json::Value::String(s) => s,
            v @ serde_json::Value::Object(_) => serde_json::to_string_pretty(&v).unwrap_or_default(),
            _ => return fail("`json` must be the project object, or that object as a JSON string"),
        };
        self.run(AgentCommand::ProjectImport {
            json: text,
            replace: a.replace.unwrap_or(false),
        })
        .await
    }

    #[tool(description = "Undo the last change (agent or user).")]
    async fn undo(&self) -> CallToolResult {
        self.run(AgentCommand::Undo).await
    }

    #[tool(description = "Redo.")]
    async fn redo(&self) -> CallToolResult {
        self.run(AgentCommand::Redo).await
    }
}

#[tool_handler]
impl ServerHandler for TtgServer {
    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        let items = RESOURCES
            .iter()
            .map(|(uri, name, desc, mime)| {
                Resource::new(*uri, *name)
                    .with_description(*desc)
                    .with_mime_type(*mime)
            })
            .collect();
        std::future::ready(Ok(ListResourcesResult::with_all_items(items)))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_ {
        let items = vec![
            ResourceTemplate::new("ttg://catalog/{type_id}", "catalog type")
                .with_description(
                    "Full definition of one abstract type (fields, relations, per-provider mapping)",
                )
                .with_mime_type("application/json"),
            ResourceTemplate::new("ttg://reach/{entity}", "reachability from an entity")
                .with_description("What one resource can reach, with paths and reasons")
                .with_mime_type("application/json"),
        ];
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(items)))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        {
            let uri = request.uri;
            if let Some(text) = doc(&uri) {
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(text, uri).with_mime_type("text/markdown")
                ])
                .into());
            }
            let cmd = match uri.as_str() {
                "ttg://project" => AgentCommand::ProjectGet,
                "ttg://project/summary" => AgentCommand::ProjectSummary,
                "ttg://diagnostics" => AgentCommand::Diagnostics,
                "ttg://catalog" => AgentCommand::CatalogTypes,
                u if u.starts_with("ttg://catalog/") => AgentCommand::CatalogType {
                    type_id: u["ttg://catalog/".len()..].to_string(),
                },
                u if u.starts_with("ttg://reach/") => AgentCommand::ReachFrom {
                    entity: u["ttg://reach/".len()..].to_string(),
                },
                _ => {
                    return Err(McpError::resource_not_found(
                        format!("unknown resource {uri}"),
                        None,
                    ))
                }
            };
            match self.exec(cmd).await {
                Ok(v) => {
                    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
                    Ok(ReadResourceResult::new(vec![
                        ResourceContents::text(text, uri).with_mime_type("application/json")
                    ])
                    .into())
                }
                Err(e) => Err(McpError::internal_error(e, None)),
            }
        }
    }

    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        let known = RESOURCES.iter().any(|(u, ..)| *u == request.uri)
            || request.uri.starts_with("ttg://catalog/")
            || request.uri.starts_with("ttg://reach/");
        let res = if known {
            let mut subs = self.subs.lock().unwrap();
            subs.retain(|(s, _, u)| !(*s == self.session && *u == request.uri));
            subs.push((self.session, context.peer.clone(), request.uri));
            Ok(())
        } else {
            Err(McpError::resource_not_found(
                format!("unknown resource {}", request.uri),
                None,
            ))
        };
        std::future::ready(res)
    }

    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        self.subs
            .lock()
            .unwrap()
            .retain(|(s, _, u)| !(*s == self.session && *u == request.uri));
        std::future::ready(Ok(()))
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
        .with_server_info(server_identity(&self.catalog_hash))
        .with_instructions(with_identity(
            &self.catalog_hash,
            "TerraTofu GUI: a visual cloud-architecture editor that generates Terraform/OpenTofu. \
             You are editing the diagram the user has open right now; they see every change as \
             you make it and every write is one undo step. Start with project_summary and \
             catalog_types. Entities can be addressed by id or by display name. Never call \
             project_save without the user asking. Prefer entity_set_parent over explicit \
             network_membership links to containers: containment implies membership. One              diagram serves every provider: tag an entity or link with `providers` to keep it              out of the other provider's export (provider-only types are tagged automatically).              Curated types cover the portable concepts; for anything else use schema_search and add              a native resource (`native:<provider>:<type>`), setting its arguments via              entity_update.extra. Extra arguments on curated types add or override provider              arguments and are validated against the schema. Use project_apply to make several              writes as one undo step, project_changes (or a subscription to ttg://project) to notice              the user's own edits, and export_diff before export_run to show what would change. \
             Writes that need the user's approval (saving, opening, a new project, exporting, \
             importing over unsaved work, deleting when the user asked for that) answer within \
             seconds with {status: \"pending_approval\", ticket, what, applied: false}: nothing has \
             happened yet. Tell the user it is waiting for them in the app, then poll \
             approval_status {ticket} every few seconds until it says applied (with the result), \
             failed, denied or expired; never repeat the original call. Other calls keep working \
             meanwhile. Every reply says whether anything was applied: a call that comes back \
             \"the app is busy\" was never queued, and one that times out was withdrawn before it \
             ran, so either is safe to retry once; nothing is ever applied twice. \
             Pass `verbose: false` to entity_add, entity_update or link_add (or settings_set \
             { verbose: false } once) to get only status, id and changed back. \
             The .ttg.json file is the interchange format (docs/FILE_FORMAT.md, JSON schema at \
             https://raw.githubusercontent.com/geawilliamsUK/TerraTofuGUi/master/schemas/project.schema.json): \
             project_import {json, replace} loads one in memory, checking it against the schema \
             first; without this server an agent can write the file for the user to open or run \
             `ttg check --schema` / `ttg export` on.              Views: view_set while a saved view is active rewrites that view's filter (use              view_activate All first to filter without touching it), view_update takes a filter of              its own, view_save refuses a name already in use unless replace is true, and              view_delete removes one.              `terratofu-gui --serve --port N --token T [project]` runs the same server headless, with \
             no window and no approval prompts, for scripting and CI; with --oauth and \
             --public-url behind an HTTPS tunnel it is reachable from cloud sessions.",
        ))
    }
}

/// The tool an export reply says it wrote for.
fn tool_of(export: &serde_json::Value) -> ttg_core::Tool {
    if export.get("tool").and_then(|t| t.as_str()) == Some("terraform") {
        ttg_core::Tool::Terraform
    } else {
        ttg_core::Tool::OpenTofu
    }
}

/// `<tool> init && validate -json` in `dir`, off the async threads: the summary line and
/// every diagnostic attributed to its entity, file and line.
async fn run_validate(
    dir: String,
    tool: ttg_core::Tool,
    a: ttg_codegen::plan_run::Attribution,
) -> serde_json::Value {
    tokio::task::spawn_blocking(move || {
        match ttg_codegen::plan_run::validate(std::path::Path::new(&dir), tool, &a) {
            Ok((valid, diagnostics)) => serde_json::json!({
                "summary": if valid {
                    "validate passed".to_string()
                } else {
                    format!(
                        "validate FAILED: {} error(s)",
                        diagnostics.iter().filter(|d| d.severity == "error").count()
                    )
                },
                "valid": valid,
                "diagnostics": diagnostics,
            }),
            Err(e) => serde_json::json!({ "summary": e, "valid": false, "diagnostics": [] }),
        }
    })
    .await
    .unwrap_or_else(
        |e| serde_json::json!({ "summary": format!("validate task failed: {e}"), "valid": false }),
    )
}

/// `export_run { validate: true }` behind an approval prompt: wait for the ticket, and
/// once the export is applied, validate it and add the outcome to the ticket's result
/// (`running` meanwhile, so a poll in between does not mistake it for skipped).
async fn validate_after_approval(
    approvals: Arc<Approvals>,
    ticket: String,
    dir: String,
    attribution: ttg_codegen::plan_run::Attribution,
) {
    loop {
        let Some(t) = approvals.get(&ticket) else {
            return;
        };
        match t.outcome {
            super::approvals::Outcome::Pending => tokio::time::sleep(Duration::from_millis(500)).await,
            super::approvals::Outcome::Applied(result) => {
                approvals.amend(&ticket, "validate", serde_json::json!("running"));
                let outcome = run_validate(dir, tool_of(&result), attribution).await;
                approvals.amend(&ticket, "validate", outcome);
                return;
            }
            _ => return,
        }
    }
}

// ------------------------------------------------------------------ identity

/// TerraTofu's own version, from this crate's manifest — not rmcp's, which is what
/// `serverInfo` used to report and which says nothing about the tools behind it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `serverInfo`: the app's name and version, with the fingerprint of the loaded
/// definitions as semver build metadata (`0.1.0+catalog.3f9c1a7d20be`). A client that
/// cached the tool list from an older build sees the version or the hash move, and
/// knows to ask `tools/list` again.
fn server_identity(catalog_hash: &str) -> Implementation {
    Implementation::new("terratofu-gui", format!("{VERSION}+catalog.{catalog_hash}"))
        .with_title(format!("TerraTofu GUI {VERSION}"))
        .with_description(format!("TerraTofu GUI {VERSION}, catalog {catalog_hash}"))
}

/// The instructions an agent reads once at connect: who it is talking to and when to
/// refresh, then the standing advice, then the scoped calls (slices, previews, dry
/// runs, bulk writes).
fn with_identity(catalog_hash: &str, standing: &str) -> String {
    format!(
        "TerraTofu GUI {VERSION}, catalog {catalog_hash}: if your tool list lacks view_delete or \
         entity_preview, refresh it (tools/list) — schemas cached from an older build are missing \
         parameters and tools. {standing} \
         Bulk and preview: project_apply {{ dry_run: true }} plays a batch and reports the diagnostics \
         it would add or remove, then takes it all back; entity_update and link_add take \
         `select` ({{ types, name_glob, ids }}) to act on many entities as one undo step \
         (all-or-nothing, an empty or non-matching selection is refused); entity_preview shows one \
         entity's HCL; diagnostics takes entity / severity / provider, project_get takes fields / \
         entities, catalog_relations answers which relations a type can have to another; \
         view_flow_add warns when an end is hidden in the view (show_hidden: true shows it); \
         view_get gives a note's absolute `position` and, when anchored, its `offset`."
    )
}

// ------------------------------------------------------------------ runtime thread

/// How the server listens and who it lets in.
pub struct ServeOptions {
    pub port: u16,
    /// Listen address; 127.0.0.1 unless the user chose otherwise.
    pub bind: std::net::IpAddr,
    /// The bearer token local clients send (`claude mcp add --header ...`).
    pub token: String,
    /// The HTTPS origin a tunnel publishes the server under, without a trailing slash.
    pub public_url: Option<String>,
    /// OAuth sign-in, when it is on: its endpoints are served and its access tokens are
    /// accepted beside the bearer token.
    pub oauth: Option<Arc<oauth::OAuth>>,
    pub approvals: Arc<Approvals>,
}

impl ServeOptions {
    /// The Host headers accepted: this machine's names, plus the public URL's host. rmcp
    /// refuses any other to stop DNS-rebinding attacks on a local server, and the OAuth
    /// endpoints use the same list.
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        let endpoint = oauth::Endpoint {
            public_url: self.public_url.clone(),
        };
        if let Some(auth) = endpoint.public_authority() {
            let name = auth.split(':').next().unwrap_or("").to_string();
            hosts.push(name);
            hosts.push(auth);
        }
        // Bound to one outside address: clients on the network use it as the host.
        if !self.bind.is_loopback() && !self.bind.is_unspecified() {
            hosts.push(self.bind.to_string());
        }
        hosts.dedup();
        hosts
    }
}

/// Whether a request's Host is one of `allowed` (a bare name matches any port).
fn host_allowed(headers: &axum::http::HeaderMap, allowed: &[String]) -> bool {
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<axum::http::uri::Authority>().ok())
    else {
        return false;
    };
    let name = host.host().trim_matches(['[', ']']).to_ascii_lowercase();
    allowed
        .iter()
        .any(|a| match a.parse::<axum::http::uri::Authority>() {
            Ok(allowed) => {
                allowed
                    .host()
                    .trim_matches(['[', ']'])
                    .eq_ignore_ascii_case(&name)
                    && allowed.port_u16().is_none_or(|p| host.port_u16() == Some(p))
            }
            Err(_) => a.trim_matches(['[', ']']).eq_ignore_ascii_case(&name),
        })
}

/// Body of the server thread: bind, serve until the cancellation token fires.
pub fn run(
    opts: ServeOptions,
    tx: mpsc::Sender<(AgentCommand, AgentReply)>,
    ctx: egui::Context,
    beat: Arc<Heartbeat>,
    catalog_hash: String,
    started: mpsc::Sender<Result<Started, String>>,
) {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = started.send(Err(format!("tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        let port = opts.port;
        let bind = opts.bind;
        let listener = match tokio::net::TcpListener::bind((bind, port)).await {
            Ok(l) => l,
            Err(e) => {
                let hint = if e.raw_os_error() == Some(10013) {
                    " (Windows reserves this port range, e.g. for Hyper-V; pick another port in Agent settings)"
                } else if e.kind() == std::io::ErrorKind::AddrInUse {
                    " (already in use: another app or another TerraTofu window?)"
                } else {
                    ""
                };
                let _ = started.send(Err(format!("cannot listen on {bind}:{port}: {e}{hint}")));
                return;
            }
        };
        let allowed = opts.allowed_hosts();
        let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed.clone());
        let cancel = config.cancellation_token.clone();
        let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let subs_for_service = subs.clone();
        let approvals = opts.approvals.clone();
        let service = StreamableHttpService::new(
            move || {
                Ok(TtgServer::new(
                    tx.clone(),
                    ctx.clone(),
                    subs_for_service.clone(),
                    beat.clone(),
                    catalog_hash.clone(),
                    approvals.clone(),
                ))
            },
            std::sync::Arc::new(LocalSessionManager::default()),
            config,
        );
        // Fan project changes out to subscribed clients, coalescing bursts (drags).
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                let ServerEvent::ProjectChanged = ev;
                tokio::time::sleep(Duration::from_millis(250)).await;
                while ev_rx.try_recv().is_ok() {}
                let targets: Vec<(u64, Peer<RoleServer>, String)> = subs.lock().unwrap().clone();
                let mut dead = Vec::new();
                for (sid, peer, uri) in targets {
                    if uri.starts_with("ttg://docs/") || uri.starts_with("ttg://catalog") {
                        continue;
                    }
                    let n = ServerNotification::ResourceUpdatedNotification(ResourceUpdatedNotification::new(
                        ResourceUpdatedNotificationParam::new(uri.clone()),
                    ));
                    if peer.send_notification(n).await.is_err() {
                        dead.push(sid);
                    }
                }
                if !dead.is_empty() {
                    subs.lock().unwrap().retain(|(s, _, _)| !dead.contains(s));
                }
            }
        });
        // `/mcp` takes the bearer token or, with OAuth on, a live access token. A 401
        // then says where to sign in (RFC 9728), which is how a connector discovers it.
        let expected = format!("Bearer {}", opts.token);
        let endpoint = oauth::Endpoint {
            public_url: opts.public_url.clone(),
        };
        let signin = opts.oauth.clone();
        let auth_endpoint = endpoint.clone();
        let auth = axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let given = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ok = given.as_deref() == Some(expected.as_str())
                || match (&signin, given.as_deref().and_then(|g| g.strip_prefix("Bearer "))) {
                    (Some(o), Some(tok)) => o.check_access(tok.trim()),
                    _ => false,
                };
            let challenge = (!ok && signin.is_some())
                .then(|| oauth::challenge(&auth_endpoint, req.headers(), given.is_some()));
            async move {
                if ok {
                    return next.run(req).await;
                }
                let mut resp = axum::response::IntoResponse::into_response((
                    axum::http::StatusCode::UNAUTHORIZED,
                    "missing or wrong bearer token (see Agent ▸ MCP settings in TerraTofu GUI)",
                ));
                let value = challenge.unwrap_or_else(|| "Bearer".to_string());
                if let Ok(v) = value.parse() {
                    resp.headers_mut().insert(axum::http::header::WWW_AUTHENTICATE, v);
                }
                resp
            }
        });
        let mut router = axum::Router::new().nest_service("/mcp", service).route_layer(auth);
        if let Some(o) = &opts.oauth {
            router = router.merge(oauth::routes(o.clone(), endpoint));
        }
        // The same Host rule for every route, the OAuth pages included.
        let host_check = axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let ok = host_allowed(req.headers(), &allowed);
            async move {
                if ok {
                    next.run(req).await
                } else {
                    axum::response::IntoResponse::into_response((
                        axum::http::StatusCode::FORBIDDEN,
                        "Forbidden: Host header is not allowed (set the public URL in Agent settings, or --public-url, to the address the tunnel uses)",
                    ))
                }
            }
        });
        let router = router.layer(host_check);
        let _ = started.send(Ok((cancel.clone(), ev_tx)));
        let shutdown = cancel.clone();
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(public_url: Option<&str>) -> ServeOptions {
        ServeOptions {
            port: 9337,
            bind: "127.0.0.1".parse().unwrap(),
            token: "t".into(),
            public_url: public_url.map(str::to_string),
            oauth: None,
            approvals: Arc::new(Approvals::default()),
        }
    }

    fn host(h: &str) -> axum::http::HeaderMap {
        let mut m = axum::http::HeaderMap::new();
        m.insert(axum::http::header::HOST, h.parse().unwrap());
        m
    }

    #[test]
    fn only_local_names_and_the_public_host_are_accepted() {
        let local = opts(None).allowed_hosts();
        assert!(host_allowed(&host("127.0.0.1:9337"), &local));
        assert!(host_allowed(&host("localhost:9337"), &local));
        assert!(host_allowed(&host("[::1]:9337"), &local));
        assert!(!host_allowed(&host("ttg.example.com"), &local));
        assert!(!host_allowed(&host("attacker.example:9337"), &local));

        let public = opts(Some("https://ttg.example.com")).allowed_hosts();
        assert!(host_allowed(&host("ttg.example.com"), &public));
        assert!(host_allowed(&host("TTG.example.com:443"), &public));
        assert!(host_allowed(&host("127.0.0.1:9337"), &public));
        assert!(!host_allowed(&host("other.example.com"), &public));
    }
}
