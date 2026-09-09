//! The rmcp server: tool definitions and the HTTP runtime thread.
//!
//! Every tool forwards an [`AgentCommand`] to the UI thread and waits for the reply.
//! The server itself holds no project state, so any number of sessions can share it.

use super::{AgentCommand, AgentReply, ServerEvent, Started};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock as Content, ErrorData as McpError, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ResourceTemplate, ResourceUpdatedNotification,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, ServerNotification,
        SubscribeRequestParams, UnsubscribeRequestParams,
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
    /// One `TtgServer` per client session.
    session: u64,
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
        "Current diagnostics for the target provider",
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
            }
        }
        "entity_update" => {
            let a: EntityUpdateArgs = parse(args)?;
            AgentCommand::EntityUpdate {
                entity: a.entity,
                name: a.name,
                config: a.config,
                provider_config: a.provider_config,
                manual: a.manual,
                providers: a.providers,
                extra: a.extra,
                extra_provider: a.extra_provider,
                extra_block: a.extra_block,
            }
        }
        "entity_move" => {
            let a: MoveArgs = parse(args)?;
            AgentCommand::EntityMove {
                entity: a.entity,
                x: a.x,
                y: a.y,
            }
        }
        "entity_resize" => {
            let a: ResizeArgs = parse(args)?;
            AgentCommand::EntityResize {
                entity: a.entity,
                w: a.w,
                h: a.h,
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
        "link_add" => {
            let a: LinkAddArgs = parse(args)?;
            AgentCommand::LinkAdd {
                source: a.source,
                target: a.target,
                relation: a.relation,
                providers: a.providers,
            }
        }
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
            let a: NameArgs = parse(args)?;
            AgentCommand::ViewSave { name: a.name }
        }
        "view_activate" => {
            let a: NameArgs = parse(args)?;
            AgentCommand::ViewActivate { name: a.name }
        }
        "view_group_add" => {
            let a: GroupAddArgs = parse(args)?;
            AgentCommand::GroupAdd {
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
                from: a.from,
                to: a.to,
                label: a.label.unwrap_or_default(),
                dashed: a.dashed.unwrap_or(false),
            }
        }
        "view_annotation_remove" => {
            let a: KeyArgs = parse(args)?;
            AgentCommand::AnnotationRemove { key: a.key }
        }
        "layout_tidy" => {
            let a: TidyArgs = parse(args)?;
            AgentCommand::LayoutTidy {
                container: a.container,
            }
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
            AgentCommand::SettingsSet {
                tool: a.tool,
                provider: a.provider,
                provider_settings: a.provider_settings,
            }
        }
        other => {
            return Err(format!(
                "`{other}` cannot be used inside project_apply (only diagram writes: entity_*, link_*, view_*, layout_*, selection_set, settings_set)"
            ))
        }
    })
}

fn ok_json(v: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
    CallToolResult::success(vec![Content::text(text)])
}

fn fail(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg.into())])
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
pub struct ProviderArg {
    #[schemars(description = "Provider id (`aws` / `azure`); defaults to the project's target provider")]
    pub provider: Option<String>,
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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EntityUpdateArgs {
    pub entity: String,
    #[schemars(description = "New display name")]
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
        description = "Extra provider arguments merged into the generated block, keyed by argument name (see schema_show). Values: JSON scalars/lists/objects, {\"$ref\": {\"entity\": \"<id or name>\", \"attr\": \"id\"}} for a reference, {\"$raw\": \"<hcl>\"} for raw HCL; null removes. For native resources this is where every argument goes"
    )]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(description = "Provider the extra arguments are for; defaults to the target provider")]
    pub extra_provider: Option<String>,
    #[schemars(description = "Block key the extra arguments apply to; defaults to the primary block")]
    pub extra_block: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SchemaSearchArgs {
    #[schemars(description = "Provider id; defaults to the target provider")]
    pub provider: Option<String>,
    #[schemars(description = "Space-separated terms matched against resource type names, e.g. `sqs queue`")]
    pub query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SchemaShowArgs {
    pub provider: Option<String>,
    #[schemars(description = "Resource type, e.g. `aws_s3_bucket_policy`")]
    pub resource: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    pub entity: String,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ResizeArgs {
    pub entity: String,
    pub w: i32,
    pub h: i32,
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
    pub source: String,
    pub target: String,
    #[schemars(
        description = "Relation key: network_membership, attribute_reference, iam_binding, attachment, sends_to, reads, logs_to, depends_on. Must be allowed by the source type's definition for the target type"
    )]
    pub relation: String,
    #[schemars(description = "Provider layers this link belongs to; omit for every provider")]
    pub providers: Option<Vec<String>>,
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
        description = "Filter object: { categories: [..], relations: [..], focus: entity, depth: n, hidden: [..], only: [..] }. Empty object shows everything"
    )]
    pub filter: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    pub name: String,
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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FlowAddArgs {
    #[schemars(description = "Resource (id or name) or group (id or label) the data comes from")]
    pub from: String,
    #[schemars(description = "Resource or group the data goes to")]
    pub to: String,
    #[schemars(description = "Arrow label, e.g. `job requests`")]
    pub label: Option<String>,
    pub dashed: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    #[schemars(description = "Group id/label or flow id/label")]
    pub key: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TidyArgs {
    #[schemars(
        description = "Only tidy the contents of this container (id or name); omit for the whole diagram"
    )]
    pub container: Option<String>,
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
pub struct ExportArgs {
    #[schemars(description = "Directory to write the project into")]
    pub dir: String,
    pub provider: Option<String>,
    #[schemars(description = "Run `<tool> init && validate` afterwards (20-60 s)")]
    pub validate: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

// ------------------------------------------------------------------ tools

impl TtgServer {
    pub fn new(tx: mpsc::Sender<(AgentCommand, AgentReply)>, ctx: egui::Context, subs: Subscribers) -> Self {
        TtgServer {
            tx,
            ctx,
            subs,
            session: SESSIONS.fetch_add(1, Ordering::Relaxed),
            tool_router: Self::tool_router(),
        }
    }

    /// Queue a command for the UI thread and wait for its answer. Writes get a long
    /// timeout because the user may be looking at an Allow / Deny prompt.
    async fn exec(&self, cmd: AgentCommand) -> Result<serde_json::Value, String> {
        let secs = if cmd.is_write() { 600 } else { 60 };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.tx.send((cmd, reply_tx)).is_err() {
            return Err("the app is shutting down".into());
        }
        self.ctx.request_repaint();
        match tokio::time::timeout(Duration::from_secs(secs), reply_rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the app dropped the request".into()),
            Err(_) => Err("timed out waiting for the app (is a dialog or an approval prompt open?)".into()),
        }
    }

    async fn run(&self, cmd: AgentCommand) -> CallToolResult {
        match self.exec(cmd).await {
            Ok(v) => ok_json(v),
            Err(e) => fail(e),
        }
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
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiffArgs {
    #[schemars(description = "Directory a previous export wrote to")]
    pub dir: String,
    pub provider: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ChangesArgs {
    #[schemars(description = "Revision returned by an earlier project_changes / project_summary call")]
    pub since: Option<u64>,
}

#[tool_router]
impl TtgServer {
    #[tool(
        description = "The whole project as JSON (same shape as the .ttg.json file): settings, containers, nodes, edges, views."
    )]
    async fn project_get(&self) -> CallToolResult {
        self.run(AgentCommand::ProjectGet).await
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
        description = "Search the provider schema for resource types (all 1,500+ AWS / 1,100+ Azure resources). Add one with entity_add using type_id `native:<provider>:<resource>`; then set its arguments with entity_update.extra."
    )]
    async fn schema_search(&self, Parameters(a): Parameters<SchemaSearchArgs>) -> CallToolResult {
        self.run(AgentCommand::SchemaSearch {
            provider: a.provider,
            query: a.query,
        })
        .await
    }

    #[tool(
        description = "Every argument and nested block of a provider resource type, with types, required flags and descriptions."
    )]
    async fn schema_show(&self, Parameters(a): Parameters<SchemaShowArgs>) -> CallToolResult {
        self.run(AgentCommand::SchemaShow {
            provider: a.provider,
            resource: a.resource,
        })
        .await
    }

    #[tool(
        description = "Current diagnostics (errors block export; warnings become manual steps) for the target provider."
    )]
    async fn diagnostics(&self) -> CallToolResult {
        self.run(AgentCommand::Diagnostics).await
    }

    #[tool(
        description = "Network posture of every resource: subnets, security group, way out of the network, internet exposure, listening port."
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
        description = "Generate the Terraform/OpenTofu files in memory and return them as text, without writing to disk. Fails with the blocking diagnostics if there are errors."
    )]
    async fn export_preview(&self, Parameters(a): Parameters<ProviderArg>) -> CallToolResult {
        self.run(AgentCommand::ExportPreview { provider: a.provider })
            .await
    }

    #[tool(
        description = "A PNG screenshot of the app window as the user currently sees it (base64 in `data`)."
    )]
    async fn screenshot(&self) -> CallToolResult {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.tx.send((AgentCommand::Screenshot, reply_tx)).is_err() {
            return fail("the app is shutting down");
        }
        self.ctx.request_repaint();
        match tokio::time::timeout(Duration::from_secs(15), reply_rx).await {
            Ok(Ok(Ok(v))) => {
                let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("").to_string();
                let meta = serde_json::json!({"width": v["width"], "height": v["height"]});
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
        })
        .await
    }

    #[tool(
        description = "Rename, set abstract/provider field values (checked against the definition) or the external flag."
    )]
    async fn entity_update(&self, Parameters(a): Parameters<EntityUpdateArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityUpdate {
            entity: a.entity,
            name: a.name,
            config: a.config,
            provider_config: a.provider_config,
            manual: a.manual,
            providers: a.providers,
            extra: a.extra,
            extra_provider: a.extra_provider,
            extra_block: a.extra_block,
        })
        .await
    }

    #[tool(description = "Move an entity to a canvas position (containers move with their contents).")]
    async fn entity_move(&self, Parameters(a): Parameters<MoveArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityMove {
            entity: a.entity,
            x: a.x,
            y: a.y,
        })
        .await
    }

    #[tool(description = "Resize a node or container.")]
    async fn entity_resize(&self, Parameters(a): Parameters<ResizeArgs>) -> CallToolResult {
        self.run(AgentCommand::EntityResize {
            entity: a.entity,
            w: a.w,
            h: a.h,
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
        description = "Link two entities. Direction: source depends on / uses target. Refused when the definition does not allow it or containment already implies it."
    )]
    async fn link_add(&self, Parameters(a): Parameters<LinkAddArgs>) -> CallToolResult {
        self.run(AgentCommand::LinkAdd {
            source: a.source,
            target: a.target,
            relation: a.relation,
            providers: a.providers,
        })
        .await
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

    #[tool(description = "Apply a canvas filter (what is shown); does not affect what is generated.")]
    async fn view_set(&self, Parameters(a): Parameters<ViewSetArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewSet { filter: a.filter }).await
    }

    #[tool(
        description = "Save the current filter as a named view tab in the project. New views own their layout: moves made while the view is active do not affect other views."
    )]
    async fn view_save(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewSave { name: a.name }).await
    }

    #[tool(
        description = "Switch to a saved view by name (`All` for everything). Groups, flows and per-view positions apply to the active view."
    )]
    async fn view_activate(&self, Parameters(a): Parameters<NameArgs>) -> CallToolResult {
        self.run(AgentCommand::ViewActivate { name: a.name }).await
    }

    #[tool(
        description = "Add a grouping box to the active view: an architecture-map annotation, never exported. Resources whose centre is inside it count as members and move with it."
    )]
    async fn view_group_add(&self, Parameters(a): Parameters<GroupAddArgs>) -> CallToolResult {
        self.run(AgentCommand::GroupAdd {
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
        description = "Add a labelled data-flow arrow to the active view between resources and/or groups. An annotation: not a dependency, never exported."
    )]
    async fn view_flow_add(&self, Parameters(a): Parameters<FlowAddArgs>) -> CallToolResult {
        self.run(AgentCommand::FlowAdd {
            from: a.from,
            to: a.to,
            label: a.label.unwrap_or_default(),
            dashed: a.dashed.unwrap_or(false),
        })
        .await
    }

    #[tool(description = "Remove a group or flow from the active view by id or label.")]
    async fn view_annotation_remove(&self, Parameters(a): Parameters<KeyArgs>) -> CallToolResult {
        self.run(AgentCommand::AnnotationRemove { key: a.key }).await
    }

    #[tool(description = "Auto-layout: columns by dependency, containers fitted to contents. Undoable.")]
    async fn layout_tidy(&self, Parameters(a): Parameters<TidyArgs>) -> CallToolResult {
        self.run(AgentCommand::LayoutTidy {
            container: a.container,
        })
        .await
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

    #[tool(description = "Change the tool, target provider or provider variables.")]
    async fn settings_set(&self, Parameters(a): Parameters<SettingsArgs>) -> CallToolResult {
        self.run(AgentCommand::SettingsSet {
            tool: a.tool,
            provider: a.provider,
            provider_settings: a.provider_settings,
        })
        .await
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
        description = "Write a complete Terraform/OpenTofu project directory for one provider, optionally validating it."
    )]
    async fn export_run(&self, Parameters(a): Parameters<ExportArgs>) -> CallToolResult {
        let dir = a.dir.clone();
        let result = self
            .run(AgentCommand::ExportRun {
                dir: a.dir,
                provider: a.provider,
            })
            .await;
        if !a.validate.unwrap_or(false) || result.is_error.unwrap_or(false) {
            return result;
        }
        // Validate off the UI thread; it can take a minute.
        let tool_name = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t.text).ok())
            .and_then(|v| v.get("tool").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .unwrap_or("opentofu".into());
        let tool = if tool_name == "terraform" {
            ttg_core::Tool::Terraform
        } else {
            ttg_core::Tool::OpenTofu
        };
        let outcome = tokio::task::spawn_blocking(move || {
            ttg_codegen::validate::run(std::path::Path::new(&dir), tool).summary()
        })
        .await
        .unwrap_or_else(|e| format!("validate task failed: {e}"));
        let mut content = result.content;
        content.push(Content::text(format!("validate: {outcome}")));
        CallToolResult::success(content)
    }

    #[tool(
        description = "Apply several diagram writes (entity_*, link_*, view_*, layout_*, selection_set, settings_set) as ONE undo step. Stops at the first failure and rolls the whole batch back. Returns each command's result."
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
        self.run(AgentCommand::Batch(cmds)).await
    }

    #[tool(
        description = "What exporting would change in an existing export directory: per-file added/removed line counts and unified diffs. Nothing is written."
    )]
    async fn export_diff(&self, Parameters(a): Parameters<DiffArgs>) -> CallToolResult {
        self.run(AgentCommand::ExportDiff {
            dir: a.dir,
            provider: a.provider,
        })
        .await
    }

    #[tool(
        description = "Has the diagram changed (by the user or the agent) since a revision? Returns the current revision, who changed it last and how long ago. Poll this between edits, or subscribe to the ttg://project resource for push notifications."
    )]
    async fn project_changes(&self, Parameters(a): Parameters<ChangesArgs>) -> CallToolResult {
        self.run(AgentCommand::Changes { since: a.since }).await
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
        .with_instructions(
            "TerraTofu GUI: a visual cloud-architecture editor that generates Terraform/OpenTofu. \
             You are editing the diagram the user has open right now; they see every change as \
             you make it and every write is one undo step. Start with project_summary and \
             catalog_types. Entities can be addressed by id or by display name. Never call \
             project_save without the user asking. Prefer entity_set_parent over explicit \
             network_membership links to containers: containment implies membership. One              diagram serves every provider: tag an entity or link with `providers` to keep it              out of the other provider's export (provider-only types are tagged automatically).              Curated types cover the portable concepts; for anything else use schema_search and add              a native resource (`native:<provider>:<type>`), setting its arguments via              entity_update.extra. Extra arguments on curated types add or override provider              arguments and are validated against the schema. Use project_apply to make several              writes as one undo step, project_changes (or a subscription to ttg://project) to notice              the user's own edits, and export_diff before export_run to show what would change. Some              writes (saving, opening, exporting, deleting) may wait for the user's approval.",
        )
    }
}

// ------------------------------------------------------------------ runtime thread

/// Body of the server thread: bind, serve until the cancellation token fires.
pub fn run(
    port: u16,
    token: String,
    tx: mpsc::Sender<(AgentCommand, AgentReply)>,
    ctx: egui::Context,
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
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                let hint = if e.raw_os_error() == Some(10013) {
                    " (Windows reserves this port range, e.g. for Hyper-V; pick another port in Agent settings)"
                } else if e.kind() == std::io::ErrorKind::AddrInUse {
                    " (already in use: another app or another TerraTofu window?)"
                } else {
                    ""
                };
                let _ = started.send(Err(format!("cannot listen on 127.0.0.1:{port}: {e}{hint}")));
                return;
            }
        };
        let config = StreamableHttpServerConfig::default();
        let cancel = config.cancellation_token.clone();
        let subs: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let subs_for_service = subs.clone();
        let service = StreamableHttpService::new(
            move || Ok(TtgServer::new(tx.clone(), ctx.clone(), subs_for_service.clone())),
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
        let expected = format!("Bearer {token}");
        let auth = axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let ok = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v == expected)
                .unwrap_or(false);
            async move {
                if ok {
                    next.run(req).await
                } else {
                    axum::response::IntoResponse::into_response((
                        axum::http::StatusCode::UNAUTHORIZED,
                        "missing or wrong bearer token (see Agent ▸ MCP settings in TerraTofu GUI)",
                    ))
                }
            }
        });
        let router = axum::Router::new().nest_service("/mcp", service).layer(auth);
        let _ = started.send(Ok((cancel.clone(), ev_tx)));
        let shutdown = cancel.clone();
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
}
