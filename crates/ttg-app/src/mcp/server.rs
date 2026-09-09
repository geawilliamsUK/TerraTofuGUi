//! The rmcp server: tool definitions and the HTTP runtime thread.
//!
//! Every tool forwards an [`AgentCommand`] to the UI thread and waits for the reply.
//! The server itself holds no project state, so any number of sessions can share it.

use super::{AgentCommand, AgentReply};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock as Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone)]
pub struct TtgServer {
    tx: mpsc::Sender<(AgentCommand, AgentReply)>,
    ctx: egui::Context,
    /// Read by the `#[tool_handler]`-generated `call_tool` / `list_tools`.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
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
    pub fn new(tx: mpsc::Sender<(AgentCommand, AgentReply)>, ctx: egui::Context) -> Self {
        TtgServer {
            tx,
            ctx,
            tool_router: Self::tool_router(),
        }
    }

    async fn run(&self, cmd: AgentCommand) -> CallToolResult {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.tx.send((cmd, reply_tx)).is_err() {
            return fail("the app is shutting down");
        }
        self.ctx.request_repaint();
        match tokio::time::timeout(Duration::from_secs(60), reply_rx).await {
            Ok(Ok(Ok(v))) => ok_json(v),
            Ok(Ok(Err(e))) => fail(e),
            Ok(Err(_)) => fail("the app dropped the request"),
            Err(_) => fail("timed out waiting for the app (is a dialog open?)"),
        }
    }
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "TerraTofu GUI: a visual cloud-architecture editor that generates Terraform/OpenTofu. \
             You are editing the diagram the user has open right now; they see every change as \
             you make it and every write is one undo step. Start with project_summary and \
             catalog_types. Entities can be addressed by id or by display name. Never call \
             project_save without the user asking. Prefer entity_set_parent over explicit \
             network_membership links to containers: containment implies membership. One              diagram serves every provider: tag an entity or link with `providers` to keep it              out of the other provider's export (provider-only types are tagged automatically).              Curated types cover the portable concepts; for anything else use schema_search and add              a native resource (`native:<provider>:<type>`), setting its arguments via              entity_update.extra. Extra arguments on curated types add or override provider              arguments and are validated against the schema.",
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
    started: mpsc::Sender<Result<tokio_util::sync::CancellationToken, String>>,
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
        let service = StreamableHttpService::new(
            move || Ok(TtgServer::new(tx.clone(), ctx.clone())),
            std::sync::Arc::new(LocalSessionManager::default()),
            config,
        );
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
        let _ = started.send(Ok(cancel.clone()));
        let shutdown = cancel.clone();
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
}
