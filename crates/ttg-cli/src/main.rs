//! `ttg` — headless companion to the GUI. Used in CI and by definition contributors.
//!
//! ```text
//! ttg check      <project.ttg.json> [--provider aws]
//! ttg export     <project.ttg.json> --provider aws --tool opentofu --out ./out/aws [--validate]
//! ttg export-all <project.ttg.json> --tool opentofu --out ./out [--zip] [--validate]
//! ttg catalog    [--definitions ./definitions]
//! ```

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use ttg_catalog::Catalog;
use ttg_codegen::Severity;
use ttg_core::Tool;

#[derive(Parser)]
#[command(name = "ttg", version, about = "TerraTofu GUI headless exporter")]
struct Cli {
    /// Load definitions from a directory instead of the built-in catalog.
    #[arg(long, global = true)]
    definitions: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum ToolArg {
    Terraform,
    Opentofu,
}

impl From<ToolArg> for Tool {
    fn from(t: ToolArg) -> Tool {
        match t {
            ToolArg::Terraform => Tool::Terraform,
            ToolArg::Opentofu => Tool::OpenTofu,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run diagnostics for a project against a provider.
    Check {
        project: PathBuf,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Export a single-provider project directory.
    Export {
        project: PathBuf,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        tool: Option<ToolArg>,
        #[arg(long)]
        out: PathBuf,
        /// Run `<tool> init && <tool> validate` afterwards if the binary is on PATH.
        #[arg(long)]
        validate: bool,
    },
    /// Show what exporting would change in an existing export directory, without
    /// writing anything. Exits 1 when there are changes.
    Diff {
        project: PathBuf,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        tool: Option<ToolArg>,
        /// The directory a previous export wrote to.
        #[arg(long)]
        out: PathBuf,
        /// Print the changed lines, not just the per-file summary.
        #[arg(long)]
        full: bool,
    },
    /// Export one complete project directory per provider under --out.
    ExportAll {
        project: PathBuf,
        #[arg(long)]
        tool: Option<ToolArg>,
        #[arg(long)]
        out: PathBuf,
        /// Also produce <out>.zip.
        #[arg(long)]
        zip: bool,
        #[arg(long)]
        validate: bool,
    },
    /// Load and validate the definition catalog, then list what it contains.
    Catalog {
        /// Also report abstract fields no provider uses, outputs naming attributes the
        /// provider schema lacks, and relations no provider consumes. Exits 1 on findings.
        #[arg(long)]
        strict: bool,
        /// Print a starter definition file for a new abstract type instead.
        #[arg(long, value_name = "TYPE_ID")]
        example: Option<String>,
    },
    /// Reachability: what is exposed, and what a resource can reach.
    Reach {
        project: PathBuf,
        #[arg(long)]
        provider: Option<String>,
        /// Name of the source resource; omit for the exposure overview.
        #[arg(long)]
        from: Option<String>,
    },
    /// The bundled / refreshed provider schema index (resources, arguments, nested blocks).
    Schema {
        #[command(subcommand)]
        cmd: SchemaCmd,
    },
    /// Re-lay out a project file automatically (columns by dependency, containers fitted).
    Tidy {
        project: PathBuf,
        /// Where to write the tidied project; defaults to overwriting the input.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Show which index is in use and what it covers.
    Info,
    /// Print the JSON Schema of the .ttg.json project file (or write it with --out).
    Project {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Regenerate the index from the installed tool (`<tool> providers schema -json`).
    /// Writes to the per-user data directory by default; `--out` writes elsewhere
    /// (use `--out crates/ttg-schema/data/index.json.gz` to refresh the bundled copy).
    Refresh {
        #[arg(long)]
        tool: Option<ToolArg>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Search resource types on a provider.
    Search {
        provider: String,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print the arguments and nested blocks of one resource type.
    Show { provider: String, resource: String },
}

fn load_catalog(dir: &Option<PathBuf>) -> Result<Catalog> {
    match dir {
        Some(d) => Catalog::load_dir(d).with_context(|| format!("loading definitions from {}", d.display())),
        None => Ok(Catalog::builtin()),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cat = load_catalog(&cli.definitions)?;
    match cli.cmd {
        Cmd::Catalog { strict, example } => {
            if let Some(t) = example {
                print!("{}", starter_definition(&t, &cat));
                return Ok(());
            }
            println!("providers:");
            for (id, p) in &cat.providers {
                println!(
                    "  {id:<8} {} ({}/{} {})",
                    p.provider.display_name,
                    p.provider.source_namespace,
                    p.provider.source_name,
                    p.provider.version_constraint
                );
            }
            println!("resources:");
            for (id, r) in &cat.resources {
                let provs: Vec<String> = r
                    .providers
                    .iter()
                    .map(|(p, m)| format!("{p}:{:?}", m.status).to_lowercase())
                    .collect();
                println!(
                    "  {id:<18} {:<18} [{}] {}",
                    r.resource.display_name,
                    r.resource.category,
                    provs.join(", ")
                );
            }
            if strict {
                let findings = strict_findings(&cat);
                if findings.is_empty() {
                    println!("strict: no findings");
                } else {
                    println!("strict: {} finding(s)", findings.len());
                    for f in &findings {
                        println!("  - {f}");
                    }
                    std::process::exit(1);
                }
            }
        }
        Cmd::Schema { cmd } => match cmd {
            SchemaCmd::Project { out } => {
                let json = ttg_core::json_schema::project_schema_json();
                match out {
                    Some(p) => {
                        std::fs::write(&p, format!("{json}\n"))?;
                        eprintln!("wrote {}", p.display());
                    }
                    None => println!("{json}"),
                }
            }
            SchemaCmd::Info => {
                let idx = ttg_schema::index();
                println!("index: {} ({})", ttg_schema::index_source(), idx.generated);
                for (id, p) in &idx.providers {
                    println!(
                        "  {id:<6} {} {}  {} resources",
                        p.source,
                        p.version,
                        p.resources.len()
                    );
                }
                if let Some(p) = ttg_schema::user_index_path() {
                    println!(
                        "user copy: {}{}",
                        p.display(),
                        if p.is_file() { "" } else { " (absent)" }
                    );
                }
            }
            SchemaCmd::Refresh { tool, out } => {
                let tool = tool.map(Into::into).unwrap_or(Tool::OpenTofu);
                let providers: Vec<(String, String)> = cat
                    .providers
                    .values()
                    .map(|p| {
                        (
                            format!("{}/{}", p.provider.source_namespace, p.provider.source_name),
                            p.provider.version_constraint.clone(),
                        )
                    })
                    .collect();
                let ids: std::collections::BTreeMap<String, String> = cat
                    .providers
                    .iter()
                    .map(|(id, p)| (p.provider.source_name.clone(), id.clone()))
                    .collect();
                eprintln!(
                    "running {} providers schema -json (downloads providers on first use)...",
                    ttg_codegen::tool::Profile::new(tool).binary()
                );
                let raw = ttg_codegen::validate::dump_provider_schemas(tool, &providers)
                    .map_err(|e| anyhow::anyhow!(e))?;
                let (json, lock) = raw.split_once("\n//LOCK\n").unwrap_or((raw.as_str(), ""));
                let mut idx =
                    ttg_schema::compact_from_tool_json(json, &ids).map_err(|e| anyhow::anyhow!(e))?;
                for (id, p) in idx.providers.iter_mut() {
                    let name = &cat.providers[id].provider.source_name;
                    if let Some(pos) = lock.find(&format!("/{name}\"")) {
                        if let Some(v) = lock[pos..]
                            .split("version")
                            .nth(1)
                            .and_then(|s| s.split('"').nth(1))
                        {
                            p.version = v.to_string();
                        }
                    }
                }
                let dest = match out {
                    Some(o) => o,
                    None => {
                        ttg_schema::user_index_path().ok_or_else(|| anyhow::anyhow!("no data directory"))?
                    }
                };
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(&dest, idx.to_gzip().map_err(|e| anyhow::anyhow!(e))?)?;
                println!("wrote {} ({} resources)", dest.display(), idx.resource_count());
                for (id, p) in &idx.providers {
                    println!(
                        "  {id:<6} {} {}  {} resources",
                        p.source,
                        p.version,
                        p.resources.len()
                    );
                }
            }
            SchemaCmd::Search {
                provider,
                query,
                limit,
            } => {
                let idx = ttg_schema::index();
                let p = idx
                    .provider(&provider)
                    .ok_or_else(|| anyhow::anyhow!("unknown provider {provider}"))?;
                for r in p.search(&query, limit) {
                    println!("{r}");
                }
            }
            SchemaCmd::Show { provider, resource } => {
                let idx = ttg_schema::index();
                let b = idx
                    .resource(&provider, &resource)
                    .ok_or_else(|| anyhow::anyhow!("no {resource} on {provider}"))?;
                print_block(b, 0);
            }
        },
        Cmd::Tidy { project, out } => {
            let mut p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let size = |p: &ttg_core::Project, id: &str| -> ttg_core::Size {
                if let Some(c) = p.containers.get(id) {
                    return c.size;
                }
                p.nodes
                    .get(id)
                    .and_then(|n| n.size)
                    .unwrap_or(ttg_core::Size { w: 176, h: 64 })
            };
            ttg_core::layout::tidy(&mut p, &size, &ttg_core::layout::TidyOptions::default());
            let dest = out.unwrap_or(project);
            ttg_core::project::save(&p, &dest)?;
            println!("tidied -> {}", dest.display());
        }
        Cmd::Reach {
            project,
            provider,
            from,
        } => {
            let p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let provider = provider.unwrap_or(p.settings.target_provider.clone());
            let reach = ttg_codegen::reach::analyse(&p, &cat, &provider);
            let name = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or(id.to_string());
            match from {
                None => {
                    for (id, po) in &reach.posture {
                        let egress = match &po.egress {
                            ttg_codegen::reach::Egress::Unrestricted => "outside network".to_string(),
                            ttg_codegen::reach::Egress::NotNeeded => {
                                "passive (no outbound needed)".to_string()
                            }
                            ttg_codegen::reach::Egress::Via(h) => {
                                format!(
                                    "egress via {}",
                                    h.iter().map(|x| name(x)).collect::<Vec<_>>().join(" > ")
                                )
                            }
                            ttg_codegen::reach::Egress::Blocked(r) => format!("NO EGRESS ({r})"),
                        };
                        let exposed = po
                            .exposed
                            .as_deref()
                            .map(|x| format!(" | EXPOSED: {x}"))
                            .unwrap_or_default();
                        println!("{:<22} {egress}{exposed}", name(id));
                    }
                }
                Some(from) => {
                    let src = p
                        .entities()
                        .into_iter()
                        .find(|e| e.name.eq_ignore_ascii_case(&from))
                        .map(|e| e.id.to_string())
                        .with_context(|| format!("no resource named '{from}'"))?;
                    for path in ttg_codegen::reach::paths_from(&p, &cat, &reach, &src) {
                        let status = match path.status {
                            ttg_codegen::reach::Status::Ok => "OK     ",
                            ttg_codegen::reach::Status::Blocked => "BLOCKED",
                            ttg_codegen::reach::Status::Unknown => "UNKNOWN",
                        };
                        let hops = if path.hops.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " [{}]",
                                path.hops.iter().map(|x| name(x)).collect::<Vec<_>>().join(" > ")
                            )
                        };
                        println!("{status} -> {:<20} {}{hops}", name(&path.target), path.reason);
                        for n in &path.notes {
                            println!("           note: {n}");
                        }
                    }
                }
            }
        }
        Cmd::Check { project, provider } => {
            let p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let provider = provider.unwrap_or(p.settings.target_provider.clone());
            let diags = ttg_codegen::diagnostics::run(&p, &cat, &provider);
            for d in &diags {
                println!("{d}");
            }
            let errors = diags.iter().filter(|d| d.severity == Severity::Error).count();
            println!("{} diagnostics, {errors} error(s)", diags.len());
            if errors > 0 {
                std::process::exit(1);
            }
        }
        Cmd::Export {
            project,
            provider,
            tool,
            out,
            validate,
        } => {
            let p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let provider = provider.unwrap_or(p.settings.target_provider.clone());
            let tool: Tool = tool.map(Into::into).unwrap_or(p.settings.tool);
            let rep = ttg_codegen::export(&p, &cat, &provider, tool, &out)?;
            print_report(&rep);
            if validate {
                let o = ttg_codegen::validate::run(&out, tool);
                println!("{}", o.summary());
                if matches!(o, ttg_codegen::validate::Outcome::Ran { success: false, .. }) {
                    std::process::exit(2);
                }
            }
        }
        Cmd::Diff {
            project,
            provider,
            tool,
            out,
            full,
        } => {
            let p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let provider = provider.unwrap_or(p.settings.target_provider.clone());
            let tool: Tool = tool.map(Into::into).unwrap_or(p.settings.tool);
            let g = ttg_codegen::generate(&p, &cat, &provider, tool)?;
            let diffs = ttg_codegen::diff::against_dir(&g, &out);
            println!(
                "[{provider}] {} vs {}: {}",
                tool.display_name(),
                out.display(),
                ttg_codegen::diff::summary(&diffs)
            );
            for d in diffs.iter().filter(|d| d.changed()) {
                let tag = match d.status {
                    ttg_codegen::diff::FileStatus::Added => "added  ",
                    ttg_codegen::diff::FileStatus::Removed => "removed",
                    _ => "changed",
                };
                println!("  {tag} {:<20} +{} -{}", d.name, d.added, d.removed);
                if full {
                    print!("{}", ttg_codegen::diff::render(d, 2));
                }
            }
            if diffs.iter().any(|d| d.changed()) {
                std::process::exit(1);
            }
        }
        Cmd::ExportAll {
            project,
            tool,
            out,
            zip,
            validate,
        } => {
            let p = ttg_core::project::load(&project)?;
            cat.ensure_native_types(&p);
            let tool: Tool = tool.map(Into::into).unwrap_or(p.settings.tool);
            std::fs::create_dir_all(&out)?;
            let results = ttg_codegen::export_all(&p, &cat, tool, &out);
            ttg_codegen::bundle::write_bundle_readme(&out, &p.name, &results)?;
            let mut failed = false;
            for (pid, r) in &results {
                match r {
                    Ok(rep) => {
                        print_report(rep);
                        if validate {
                            let o = ttg_codegen::validate::run(&rep.out_dir, tool);
                            println!("  [{pid}] {}", o.summary());
                            if matches!(o, ttg_codegen::validate::Outcome::Ran { success: false, .. }) {
                                failed = true;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[{pid}] export failed: {e}");
                        failed = true;
                    }
                }
            }
            if zip {
                let zp = out.with_extension("zip");
                ttg_codegen::bundle::zip_dir(&out, &zp)?;
                println!("bundle: {}", zp.display());
            }
            if failed {
                bail!("one or more providers failed");
            }
        }
    }
    Ok(())
}

fn print_report(rep: &ttg_codegen::ExportReport) {
    println!(
        "[{}] {} -> {}",
        rep.provider,
        rep.tool.display_name(),
        rep.out_dir.display()
    );
    for f in &rep.files {
        println!("  wrote {f}");
    }
    for w in &rep.warnings {
        println!("  {w}");
    }
    if !rep.manual_steps.is_empty() {
        println!(
            "  {} manual step(s) — see MANUAL_STEPS.md",
            rep.manual_steps.len()
        );
    }
}

fn print_block(b: &ttg_schema::BlockSchema, indent: usize) {
    let pad = "  ".repeat(indent);
    for (k, a) in &b.attributes {
        let flag = if a.required() {
            "required"
        } else if a.read_only() {
            "read-only"
        } else {
            "optional"
        };
        println!(
            "{pad}{k:<32} {:<10} {flag}  {}",
            a.kind().label(),
            a.description()
        );
    }
    for (k, n) in &b.blocks {
        println!(
            "{pad}{k} {{  # nested block, {}{}",
            n.nesting(),
            if n.required() { ", required" } else { "" }
        );
        print_block(n.block(), indent + 1);
        println!("{pad}}}");
    }
}

/// Things a definition can get wrong without producing invalid HCL: dead abstract
/// fields, outputs that name attributes the provider does not have, relations that no
/// provider mapping ever consumes.
fn strict_findings(cat: &ttg_catalog::Catalog) -> Vec<String> {
    let idx = ttg_schema::index();
    let mut out = Vec::new();
    for (id, r) in &cat.resources {
        if ttg_catalog::Catalog::is_native(id) {
            continue;
        }
        for f in &r.fields {
            if ttg_catalog::usage::providers_using(r, &f.name).is_empty() {
                out.push(format!(
                    "{id}: abstract field '{}' is used by no provider mapping",
                    f.name
                ));
            }
        }
        for (pid, m) in &r.providers {
            let primary = m
                .blocks
                .iter()
                .find(|b| b.key == "main")
                .or(m.blocks.first())
                .map(|b| b.key.clone());
            for (oname, o) in &m.outputs {
                let key = o.block.clone().or(primary.clone());
                let Some(b) = m.blocks.iter().find(|b| Some(&b.key) == key.as_ref()) else {
                    out.push(format!(
                        "{id}/{pid}: output '{oname}' references block '{}' which the mapping does not define",
                        key.unwrap_or_default()
                    ));
                    continue;
                };
                if b.resource == "terraform_data" {
                    continue;
                }
                let root = o.attr.split('.').next().unwrap_or(&o.attr);
                if let Some(schema) = idx.resource(pid, &b.resource) {
                    if !schema.has(root) {
                        out.push(format!(
                            "{id}/{pid}: output '{oname}' names attribute '{}' which {} does not have",
                            o.attr, b.resource
                        ));
                    }
                }
            }
        }
        for rel in &r.relations {
            let applicable: Vec<&String> = r
                .providers
                .keys()
                .filter(|pid| rel.providers.is_empty() || rel.providers.contains(pid))
                .collect();
            let consumed = applicable.iter().any(|pid| {
                ttg_codegen::diagnostics::consumed_relations(&r.providers[*pid])
                    .iter()
                    .any(|c| c.relation == rel.kind)
            });
            if !consumed && !applicable.is_empty() && rel.kind != "depends_on" {
                out.push(format!(
                    "{id}: relation '{}' ({}) is consumed by no provider mapping",
                    rel.kind,
                    rel.label.clone().unwrap_or_default()
                ));
            }
        }
    }
    out
}

/// A starter definition for a new abstract type, with every provider section stubbed.
fn starter_definition(type_id: &str, cat: &ttg_catalog::Catalog) -> String {
    let display: String = type_id
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut s = format!(
        "# Abstract type: {display}  (schema_version 2)\n#\n# One-paragraph description of the concept and how each provider renders it.\n\nschema_version = 2\n\n[resource]\ntype = \"{type_id}\"\ncategory = \"compute\"          # compute | network | storage | database | serverless | container | dns | secrets | monitoring | iam\ndisplay_name = \"{display}\"\ndescription = \"What it is and which links matter, in one sentence for the palette tooltip.\"\nkind = \"node\"                 # node | container\nallowed_parents = [\"resource_group\", \"virtual_network\"]\nicon = \"{}\"\n\n[[fields]]\nname = \"size\"\nlabel = \"Size\"\ntype = \"enum\"                 # string | int | bool | cidr | enum | string_list | struct_list | entity_ref\nrequired = true\noptions = [\"small\", \"medium\", \"large\"]\ndefault = \"small\"\n\n[[relations]]\nkind = \"network_membership\"   # network_membership | attribute_reference | iam_binding | attachment | sends_to | reads | logs_to\nlabel = \"Subnets\"\ntargets = [\"subnet\"]\ncardinality = \"many\"          # one | optional | many\n",
        type_id.chars().take(3).collect::<String>().to_uppercase()
    );
    for (pid, p) in &cat.providers {
        let prefix = match pid.as_str() {
            "aws" => "aws_",
            "azure" => "azurerm_",
            "gcp" => "google_",
            _ => "",
        };
        s.push_str(&format!(
            "\n# ---------------------------------------------------------------- {}\n[providers.{pid}]\nstatus = \"partial\"             # full | partial (needs manual_steps) | logical (nothing generated)\n\n[[providers.{pid}.blocks]]\nkey = \"main\"\nresource = \"{prefix}{type_id}\"   # check with: ttg schema show {pid} {prefix}{type_id}\n\n[providers.{pid}.blocks.args]\nname = {{ field = \"name\", transform = \"kebab\" }}\n\n[providers.{pid}.outputs]\nid = {{ attr = \"id\", description = \"Resource id\" }}\n\n[[providers.{pid}.manual_steps]]\ntitle = \"What the mapping cannot express\"\nbody = \"\"\"\nExplain what the operator must do by hand after apply, or delete this step and set\nstatus = \"full\".\n\"\"\"\n",
            p.provider.display_name
        ));
    }
    s
}
