//! Tools for reviewing and reshaping a design: the architecture diff, named
//! checkpoints, patterns and importing an edited export directory. A second tool
//! router, merged into the server's in `TtgServer::new`.

use super::{fail, AgentCommand, TtgServer};
use rmcp::{handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProjectDiffArgs {
    #[schemars(
        description = "What to compare the open project with: \"saved\" (the project file on disk), the name of a checkpoint, or the path of another .ttg.json file"
    )]
    pub against: String,
    #[schemars(description = "`json`, `markdown` or `both` (default)")]
    pub format: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CheckpointCreateArgs {
    #[schemars(description = "Name to keep the current state under, e.g. \"before queues\"")]
    pub name: String,
    #[schemars(description = "Overwrite a checkpoint of the same name (otherwise refused)")]
    pub replace: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CheckpointNameArgs {
    #[schemars(description = "Checkpoint name (case-insensitive)")]
    pub name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PatternApplyArgs {
    #[schemars(
        description = "Pattern name from pattern_list, e.g. \"hardened bucket\" (case and punctuation ignored)"
    )]
    pub name: String,
    #[schemars(
        description = "Parameter values by name (pattern_list lists them with their defaults): text, or for an entity parameter an entity id or name (a list of them for a list parameter)"
    )]
    pub params: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(
        description = "Where the pattern's top-left goes; defaults to a free spot inside `parent` or right of the diagram"
    )]
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[schemars(
        description = "Container (id or name) to insert into, e.g. the Resource Group; each entity goes into the nearest container its type allows"
    )]
    pub parent: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PatternSaveArgs {
    #[schemars(description = "Name of the new pattern")]
    pub name: String,
    #[schemars(
        description = "Entities (ids or names) to save, with the links among them; links to entities outside become entity parameters"
    )]
    pub entities: Vec<String>,
    pub description: Option<String>,
    #[schemars(description = "Overwrite a saved pattern of the same name")]
    pub replace: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImportDirArgs {
    #[schemars(description = "Directory a TerraTofu export was written to, edited by hand since")]
    pub dir: String,
    #[schemars(description = "Provider it was exported for; read from the files' header when left out")]
    pub provider: Option<String>,
    #[schemars(
        description = "false (default): only report what would change. true: apply it as one undo step (the user is asked first in the app)"
    )]
    pub apply: Option<bool>,
}

#[tool_router(router = design_router, vis = "pub(super)")]
impl TtgServer {
    #[tool(
        description = "What changed in architecture terms between a version and the open project: entities added, removed, renamed (matched by id, then name and type) or moved between containers, field / provider-field / extra-argument / adoption changes (old -> new), links added and removed, views and their groups, flows, notes and logical nodes, and settings. Canvas positions are ignored. Returns the structure as JSON and as Markdown ready for a pull-request description. `against`: \"saved\", a checkpoint name, or a .ttg.json path."
    )]
    async fn project_diff(&self, Parameters(a): Parameters<ProjectDiffArgs>) -> CallToolResult {
        self.run(AgentCommand::ProjectDiff {
            against: a.against,
            format: a.format,
        })
        .await
    }

    #[tool(
        description = "Keep the project as it is now under a name, to come back to (checkpoint_restore) or diff against (project_diff) after a large batch. Stored beside the project file in <name>.checkpoints.json (in memory until a never-saved project is saved), never in the export. The diagram does not change."
    )]
    async fn checkpoint_create(&self, Parameters(a): Parameters<CheckpointCreateArgs>) -> CallToolResult {
        self.run(AgentCommand::CheckpointCreate {
            name: a.name,
            replace: a.replace.unwrap_or(false),
        })
        .await
    }

    #[tool(
        description = "The project's checkpoints: name, when, size, and what restoring each would change."
    )]
    async fn checkpoint_list(&self) -> CallToolResult {
        self.run(AgentCommand::CheckpointList).await
    }

    #[tool(
        description = "Replace the diagram with a checkpoint, as one undo step (undo brings the current one back). When the diagram has unsaved changes the user is asked first (the reply is then an approval ticket). Returns what changed."
    )]
    async fn checkpoint_restore(&self, Parameters(a): Parameters<CheckpointNameArgs>) -> CallToolResult {
        self.run(AgentCommand::CheckpointRestore { name: a.name }).await
    }

    #[tool(
        description = "Patterns that can be inserted: the built-ins (hardened bucket; queue with dead-letter queue and alarms; queue-driven GPU worker; web service behind a load balancer) and the user's saved ones, each with its parameters (defaults, entity parameters and the type they take) and the entities it adds."
    )]
    async fn pattern_list(&self) -> CallToolResult {
        self.run(AgentCommand::PatternList).await
    }

    #[tool(
        description = "Insert a pattern: its entities and links are added through the same commands as project_apply (types, parents, field values and relations checked the same way), as ONE undo step, placed from x/y (or a free spot) inside `parent`. A parameter that is unknown, missing or names the wrong kind of entity, or a name that collides, is refused before anything changes. Returns the entities added with their ids and the diagnostics on them."
    )]
    async fn pattern_apply(&self, Parameters(a): Parameters<PatternApplyArgs>) -> CallToolResult {
        self.run(AgentCommand::PatternApply {
            name: a.name,
            params: a.params.unwrap_or_default(),
            x: a.x,
            y: a.y,
            parent: a.parent,
        })
        .await
    }

    #[tool(
        description = "Save entities (and the links among them) as a pattern in the user's patterns folder. Names that share leading words become a `name` parameter (and the provider values that spell them are templated); links to entities outside the selection become entity parameters. pattern_list shows it afterwards."
    )]
    async fn pattern_save(&self, Parameters(a): Parameters<PatternSaveArgs>) -> CallToolResult {
        if a.entities.is_empty() {
            return fail("give the entities to save");
        }
        self.run(AgentCommand::PatternSave {
            name: a.name,
            entities: a.entities,
            description: a.description,
            replace: a.replace.unwrap_or(false),
        })
        .await
    }

    #[tool(
        description = "Import hand edits made to a directory TerraTofu exported: each generated block's changed arguments become extra overrides on its entity (literals, $ref for references, $raw for expressions, null for deleted arguments; entity_update { extra: { <arg>: {\"$restore\": true} } } puts the mapping's value back), blocks TerraTofu does not generate (including hand-written files such as data.tf) become native entities, generated blocks missing from the directory are reported (never deleted), and what cannot be imported is listed. Without apply it changes nothing and returns the report plus the architecture diff as Markdown to show the user; with apply: true it applies the same as one undo step."
    )]
    async fn project_import_dir(&self, Parameters(a): Parameters<ImportDirArgs>) -> CallToolResult {
        self.run(AgentCommand::ProjectImportDir {
            dir: a.dir,
            provider: a.provider,
            apply: a.apply.unwrap_or(false),
        })
        .await
    }
}
