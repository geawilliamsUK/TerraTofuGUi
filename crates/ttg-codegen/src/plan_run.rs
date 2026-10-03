//! `plan` (and `validate`) from the app, with every change and every error mapped back
//! to the entity that produced it.
//!
//! [`run`] does what an operator would: `<tool> init`, `<tool> plan -json -out=…` and
//! `<tool> show -json` on the saved plan, in an export directory. The plan's
//! `resource_changes[].address` (its `[index]` dropped) is looked up in the export's
//! address map, so the answer is "Database `db`: 2 to create", not 40 lines of text; a
//! diagnostic is attributed through its `range` — the file and line the emitter knows
//! each entity's blocks occupy — or, failing that, the resource address it names.
//! [`validate`] does the same for `validate -json`.
//!
//! A plan needs the provider's credentials (it reads the account) and, for a remote
//! backend, the state store. By default the plan runs in a scratch copy of the export
//! (`.ttg-plan/`, hidden, so an export never lists or removes it) whose backend is
//! overridden to a local file: the answer is what applying would create from nothing,
//! and nothing touches the real state. `real_backend` plans against the configured
//! backend in the export directory itself. Errors that say the credentials are missing
//! are recognised and reported as such ([`PlanStatus::NoCredentials`]), not as a failed
//! mapping.

use crate::emit::Generated;
use crate::tool::Profile;
use crate::validate::{cli_config_file, find_binary, plugin_cache_dir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use ttg_catalog::Catalog;
use ttg_core::{Id, Project, Tool};

/// Where each entity's resources and blocks are in an export: what attributes a plan's
/// addresses and a diagnostic's file and line to entities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attribution {
    /// `aws_db_instance.db` → entity id.
    pub addresses: BTreeMap<String, Id>,
    /// `(file, first line, last line, entity)`, 1-based and inclusive.
    pub sections: Vec<(String, usize, usize, Id)>,
    /// Entity id → display name.
    pub names: BTreeMap<Id, String>,
    /// Entity id → the definition file of its type, for whoever maintains the mapping.
    pub definitions: BTreeMap<Id, String>,
}

impl Attribution {
    /// From an export and the project and catalog it was generated from.
    pub fn of(g: &Generated, p: &Project, cat: &Catalog) -> Attribution {
        let mut a = Attribution {
            addresses: g.address_map(),
            ..Default::default()
        };
        for (id, b) in &g.entity_blocks {
            a.sections
                .push((b.file.clone(), b.lines.0, b.lines.1, id.clone()));
            if let Some(e) = p.entity(id) {
                a.names.insert(id.clone(), e.name.to_string());
                if !Catalog::is_native(e.resource_type) && cat.resource(e.resource_type).is_some() {
                    a.definitions.insert(
                        id.clone(),
                        format!("definitions/resources/{}.toml", e.resource_type),
                    );
                }
            }
        }
        a
    }

    /// The entity a resource address belongs to: `module.x.` and `[index]` parts are
    /// dropped (`aws_subnet.web_0` stays as it is; `aws_nat_gateway.nat[0]` is
    /// `aws_nat_gateway.nat`).
    pub fn entity_of_address(&self, address: &str) -> Option<&Id> {
        let mut a = address;
        while let Some(rest) = a.strip_prefix("module.") {
            a = rest.split_once('.').map(|(_, r)| r).unwrap_or(rest);
        }
        let base = match a.find('[') {
            Some(i) => &a[..i],
            None => a,
        };
        self.addresses.get(base)
    }

    /// The entity whose blocks cover `line` of `file`.
    pub fn entity_at(&self, file: &str, line: usize) -> Option<&Id> {
        let file = file.replace('\\', "/");
        let file = file.trim_start_matches("./");
        self.sections
            .iter()
            .find(|(f, a, b, _)| f == file && *a <= line && line <= *b)
            .map(|(_, _, _, id)| id)
    }
}

/// One error or warning from the tool, attributed where it can be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    /// `error` or `warning`.
    pub severity: String,
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// The resource address the diagnostic names, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_name: Option<String>,
    /// The definition file that produced the block, for a mapping bug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

/// One resource's planned change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddressChange {
    pub address: String,
    /// The plan's own actions: `["create"]`, `["delete", "create"]`, `["no-op"]`, …
    pub actions: Vec<String>,
    /// What they amount to: `create`, `update`, `delete`, `replace`, `read` or `no_op`.
    pub change: String,
}

/// Every planned change of one entity, counted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntityChanges {
    pub entity: Id,
    pub name: String,
    pub create: usize,
    pub update: usize,
    pub delete: usize,
    pub replace: usize,
    pub read: usize,
    pub no_op: usize,
    pub addresses: Vec<AddressChange>,
}

impl EntityChanges {
    /// The badge the canvas draws: `+` creates only, `~` changes, `−` deletes only,
    /// `±` replaces or a mix; `None` when nothing changes.
    pub fn badge(&self) -> Option<&'static str> {
        let changes = [self.create, self.update, self.delete, self.replace];
        match changes {
            [0, 0, 0, 0] => None,
            [_, 0, 0, 0] => Some("+"),
            [0, _, 0, 0] => Some("~"),
            [0, 0, _, 0] => Some("\u{2212}"),
            _ => Some("\u{b1}"),
        }
    }
}

/// How far a plan got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// The plan ran; `entities` holds its changes.
    Planned,
    /// The provider could not authenticate: the configuration is fine as far as the
    /// tool got, but planning needs credentials for the account.
    NoCredentials,
    /// `init` or `plan` failed for another reason; see `diagnostics`.
    Failed,
    /// Neither `tofu` nor `terraform` (as configured) was found.
    BinaryNotFound,
}

/// What [`run`] found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanReport {
    pub status: PlanStatus,
    pub tool: Tool,
    /// The directory the tool ran in.
    pub dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub var_file: Option<String>,
    /// Totals over every entity.
    pub create: usize,
    pub update: usize,
    pub delete: usize,
    pub replace: usize,
    pub entities: Vec<EntityChanges>,
    /// Changes whose address no entity produced (none, unless the directory holds files
    /// of its own).
    pub unattributed: Vec<AddressChange>,
    pub diagnostics: Vec<ToolDiagnostic>,
    /// One line: "12 to create, 0 to change, 0 to destroy" or why there is no plan.
    pub summary: String,
}

/// How to plan.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// `-var-file`, relative to the export directory (`environments/prod.tfvars`).
    pub var_file: Option<String>,
    /// `-backend-config` for a real-backend plan (`environments/prod.backend.hcl`).
    pub backend_config: Option<String>,
    /// Plan against the configured backend in the export directory itself, instead of
    /// a scratch copy with local state.
    pub real_backend: bool,
    /// `-var name=value` pairs, for variables the files leave unset (a password).
    pub vars: BTreeMap<String, String>,
}

/// The scratch copy a plan without the real backend runs in.
pub const SCRATCH_DIR: &str = ".ttg-plan";

/// The override file that points the scratch copy's backend at a local file.
const BACKEND_OVERRIDE: &str = "zz_ttg_local_backend_override.tf";

/// What an export directory written from `p` attributes to whom (the project is
/// generated again, in memory, which is cheap next to `init`).
pub fn attribution_for(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    tool: Tool,
) -> Result<Attribution, crate::GenError> {
    let g = crate::generate(p, cat, provider, tool)?;
    Ok(Attribution::of(&g, p, cat))
}

/// The environment a plan is for: the one asked for, else the first; `None` for a
/// project without environments. An unknown name is an error naming the right ones.
pub fn environment_for(p: &Project, asked: Option<&str>) -> Result<Option<String>, String> {
    let envs = &p.settings.environments;
    match asked {
        None => Ok(envs.first().cloned()),
        Some(e) if envs.iter().any(|x| x == e) => Ok(Some(e.to_string())),
        Some(e) if envs.is_empty() => Err(format!("the project has no environments, so there is no \"{e}\"")),
        Some(e) => Err(format!(
            "no environment \"{e}\" (environments: {})",
            envs.join(", ")
        )),
    }
}

/// Export `p` into `dir` and plan it for one environment: the export's
/// `environments/<env>.tfvars` (and, against the real backend, its `.backend.hcl`).
/// What `ttg plan`, the app's Plan button and the agent's `plan_run` all do.
pub fn export_and_plan(
    p: &Project,
    cat: &Catalog,
    provider: &str,
    tool: Tool,
    dir: &Path,
    environment: Option<&str>,
    mut opts: PlanOptions,
) -> Result<PlanReport, crate::GenError> {
    let env = environment_for(p, environment).map_err(crate::GenError::Emit)?;
    let g = crate::generate(p, cat, provider, tool)?;
    let attribution = Attribution::of(&g, p, cat);
    crate::export(p, cat, provider, tool, dir)?;
    if let Some(env) = &env {
        opts.var_file
            .get_or_insert_with(|| format!("{}/{env}.tfvars", crate::environments::DIR));
        opts.backend_config
            .get_or_insert_with(|| format!("{}/{env}.backend.hcl", crate::environments::DIR));
    }
    Ok(run(dir, tool, &attribution, &opts))
}

// ------------------------------------------------------------------ parsing

/// What a plan's `resource_changes[].change.actions` amount to.
pub fn change_kind(actions: &[String]) -> &'static str {
    let has = |a: &str| actions.iter().any(|x| x == a);
    if has("create") && has("delete") {
        "replace"
    } else if has("create") {
        "create"
    } else if has("delete") {
        "delete"
    } else if has("update") {
        "update"
    } else if has("read") {
        "read"
    } else {
        "no_op"
    }
}

/// Group a `show -json` plan's resource changes by entity. Entities are in the order
/// their first change appears.
pub fn parse_plan(json: &str, a: &Attribution) -> Result<(Vec<EntityChanges>, Vec<AddressChange>), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("the plan is not JSON: {e}"))?;
    let mut by_entity: Vec<EntityChanges> = Vec::new();
    let mut unattributed = Vec::new();
    for rc in v["resource_changes"].as_array().into_iter().flatten() {
        let address = rc["address"].as_str().unwrap_or_default().to_string();
        let actions: Vec<String> = rc["change"]["actions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        let change = change_kind(&actions).to_string();
        let ac = AddressChange {
            address: address.clone(),
            actions,
            change: change.clone(),
        };
        let Some(id) = a.entity_of_address(&address) else {
            unattributed.push(ac);
            continue;
        };
        let i = match by_entity.iter().position(|e| &e.entity == id) {
            Some(i) => i,
            None => {
                by_entity.push(EntityChanges {
                    entity: id.clone(),
                    name: a.names.get(id).cloned().unwrap_or_default(),
                    ..Default::default()
                });
                by_entity.len() - 1
            }
        };
        let e = &mut by_entity[i];
        match change.as_str() {
            "create" => e.create += 1,
            "update" => e.update += 1,
            "delete" => e.delete += 1,
            "replace" => e.replace += 1,
            "read" => e.read += 1,
            _ => e.no_op += 1,
        }
        e.addresses.push(ac);
    }
    Ok((by_entity, unattributed))
}

/// One diagnostic object as `validate -json` and `plan -json` both write it.
fn diagnostic_of(d: &serde_json::Value, a: &Attribution) -> ToolDiagnostic {
    let file = d["range"]["filename"].as_str().map(str::to_string);
    let line = d["range"]["start"]["line"].as_u64().map(|l| l as usize);
    let address = d["address"].as_str().map(str::to_string);
    let entity = file
        .as_deref()
        .zip(line)
        .and_then(|(f, l)| a.entity_at(f, l))
        .or_else(|| address.as_deref().and_then(|x| a.entity_of_address(x)))
        .cloned();
    ToolDiagnostic {
        severity: d["severity"].as_str().unwrap_or("error").to_string(),
        summary: d["summary"].as_str().unwrap_or_default().to_string(),
        detail: d["detail"].as_str().unwrap_or_default().to_string(),
        file,
        line,
        address,
        entity_name: entity.as_ref().and_then(|id| a.names.get(id).cloned()),
        definition: entity.as_ref().and_then(|id| a.definitions.get(id).cloned()),
        entity,
    }
}

/// The diagnostics of a `validate -json` document.
pub fn parse_validate(json: &str, a: &Attribution) -> Result<(bool, Vec<ToolDiagnostic>), String> {
    let v: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|e| format!("validate did not answer in JSON: {e}"))?;
    let diags = v["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| diagnostic_of(d, a))
        .collect();
    Ok((v["valid"].as_bool().unwrap_or(false), diags))
}

/// The diagnostics in a `plan -json` (or `init -json`) message stream: one JSON object
/// per line, the diagnostics being those of `"type": "diagnostic"`.
pub fn parse_stream(stream: &str, a: &Attribution) -> Vec<ToolDiagnostic> {
    stream
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "diagnostic")
        .map(|v| diagnostic_of(&v["diagnostic"], a))
        .collect()
}

/// Does this error say the provider has no (valid) credentials?
pub fn is_credentials_error(d: &ToolDiagnostic) -> bool {
    let text = format!("{} {}", d.summary, d.detail).to_lowercase();
    [
        "no valid credential sources",
        "failed to refresh cached credentials",
        "credentials",
        "could not find default credentials",
        "unable to build authorizer",
        "building arm config",
        "building azurerm client",
        "az login",
        "invalidclienttokenid",
        "unrecognizedclientexception",
        "authenticat",
        "retrieving aws account details",
        "shared config profile",
        "subscription_id",
        "obtain access token",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

// ------------------------------------------------------------------ running

struct Tooling {
    bin: PathBuf,
    cache: PathBuf,
    cli_config: Option<PathBuf>,
}

impl Tooling {
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(args)
            .current_dir(dir)
            .env("TF_PLUGIN_CACHE_DIR", &self.cache)
            .env("TF_IN_AUTOMATION", "1");
        if let Some(cfg) = &self.cli_config {
            c.env("TF_CLI_CONFIG_FILE", cfg).env("TOFU_CLI_CONFIG_FILE", cfg);
        }
        c
    }
}

fn output_text(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Copy the configuration (top-level `.tf` and `.tfvars` files, `environments/`, the
/// lock file) into the scratch directory, with an override that keeps the state local.
fn scratch_copy(dir: &Path) -> std::io::Result<PathBuf> {
    let scratch = dir.join(SCRATCH_DIR);
    // Leave `.terraform/` (downloaded providers) in place between runs; replace the rest.
    if let Ok(rd) = std::fs::read_dir(&scratch) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_file() && !name.starts_with('.') {
                std::fs::remove_file(e.path())?;
            }
        }
    }
    std::fs::create_dir_all(scratch.join(crate::environments::DIR))?;
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if e.path().is_file()
            && (name.ends_with(".tf") || name.ends_with(".tfvars") || name == ".terraform.lock.hcl")
        {
            std::fs::copy(e.path(), scratch.join(&name))?;
        }
    }
    if let Ok(rd) = std::fs::read_dir(dir.join(crate::environments::DIR)) {
        for e in rd.flatten() {
            if e.path().is_file() {
                std::fs::copy(
                    e.path(),
                    scratch.join(crate::environments::DIR).join(e.file_name()),
                )?;
            }
        }
    }
    std::fs::write(
        scratch.join(BACKEND_OVERRIDE),
        "# Written by TerraTofu GUI for a plan that must not touch the real state: the\n\
         # backend is a local file in this scratch copy, so the plan shows what applying\n\
         # would create from nothing.\n\
         terraform {\n  backend \"local\" {\n    path = \"terraform.tfstate\"\n  }\n}\n",
    )?;
    Ok(scratch)
}

/// `validate -json` in `dir` (after `init -backend=false`), every diagnostic attributed.
/// `Err` when the binary is missing or `init` fails, with the text to show.
pub fn validate(dir: &Path, tool: Tool, a: &Attribution) -> Result<(bool, Vec<ToolDiagnostic>), String> {
    let Some(bin) = find_binary(tool) else {
        let b = Profile::new(tool).binary();
        return Err(format!(
            "`{b}` not found on PATH. Run `{b} init -backend=false && {b} validate` in the export directory."
        ));
    };
    let cache = plugin_cache_dir();
    let t = Tooling {
        cli_config: cli_config_file(&cache),
        bin,
        cache,
    };
    let init = t
        .command(dir, &["init", "-backend=false", "-input=false", "-no-color"])
        .output()
        .map_err(|e| format!("failed to run init: {e}"))?;
    if !init.status.success() {
        return Err(format!("init failed:\n{}", output_text(&init)));
    }
    let out = t
        .command(dir, &["validate", "-json", "-no-color"])
        .output()
        .map_err(|e| format!("failed to run validate: {e}"))?;
    parse_validate(&String::from_utf8_lossy(&out.stdout), a)
}

/// `init` and `plan` in `dir` (or its scratch copy), then `show -json`; see the module docs.
pub fn run(dir: &Path, tool: Tool, a: &Attribution, opts: &PlanOptions) -> PlanReport {
    let mut report = PlanReport {
        status: PlanStatus::Failed,
        tool,
        dir: dir.to_path_buf(),
        var_file: opts.var_file.clone(),
        create: 0,
        update: 0,
        delete: 0,
        replace: 0,
        entities: Vec::new(),
        unattributed: Vec::new(),
        diagnostics: Vec::new(),
        summary: String::new(),
    };
    let Some(bin) = find_binary(tool) else {
        report.status = PlanStatus::BinaryNotFound;
        report.summary = format!("`{}` not found on PATH", Profile::new(tool).binary());
        return report;
    };
    let cache = plugin_cache_dir();
    let t = Tooling {
        cli_config: cli_config_file(&cache),
        bin,
        cache,
    };
    let work = if opts.real_backend {
        dir.to_path_buf()
    } else {
        match scratch_copy(dir) {
            Ok(d) => d,
            Err(e) => {
                report.summary = format!("could not copy the export to {SCRATCH_DIR}/: {e}");
                return report;
            }
        }
    };
    report.dir = work.clone();
    let fail = |mut report: PlanReport, what: &str, diags: Vec<ToolDiagnostic>, raw: String| {
        let errors: Vec<&ToolDiagnostic> = diags.iter().filter(|d| d.severity == "error").collect();
        report.status = if !errors.is_empty() && errors.iter().all(|d| is_credentials_error(d)) {
            PlanStatus::NoCredentials
        } else {
            PlanStatus::Failed
        };
        report.summary = match (report.status, errors.first()) {
            (PlanStatus::NoCredentials, Some(e)) => {
                format!("{what} stopped: the provider has no credentials ({})", e.summary)
            }
            // The detail's first sentence says which variable, argument or resource.
            (_, Some(e)) => match e
                .detail
                .split(". ")
                .next()
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                Some(d) => format!("{what} failed: {} ({d})", e.summary),
                None => format!("{what} failed: {}", e.summary),
            },
            (_, None) => format!(
                "{what} failed: {}",
                raw.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
            ),
        };
        if diags.is_empty() && !raw.trim().is_empty() {
            report.diagnostics.push(ToolDiagnostic {
                severity: "error".into(),
                summary: format!("{what} failed"),
                detail: raw,
                file: None,
                line: None,
                address: None,
                entity: None,
                entity_name: None,
                definition: None,
            });
        } else {
            report.diagnostics = diags;
        }
        report
    };
    let mut init_args = vec!["init", "-input=false", "-no-color"];
    let backend_arg;
    if opts.real_backend {
        if let Some(b) = &opts.backend_config {
            init_args.push("-reconfigure");
            backend_arg = format!("-backend-config={b}");
            init_args.push(&backend_arg);
        }
    }
    let init = match t.command(&work, &init_args).output() {
        Ok(o) => o,
        Err(e) => return fail(report, "init", Vec::new(), format!("failed to run init: {e}")),
    };
    if !init.status.success() {
        let text = output_text(&init);
        return fail(report, "init", Vec::new(), text);
    }
    let mut plan_args: Vec<String> = [
        "plan",
        "-input=false",
        "-no-color",
        "-json",
        "-lock=false",
        "-out=tfplan",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if opts.real_backend {
        plan_args.retain(|a| a != "-lock=false");
    }
    if let Some(v) = &opts.var_file {
        plan_args.push(format!("-var-file={v}"));
    }
    for (k, v) in &opts.vars {
        plan_args.push(format!("-var={k}={v}"));
    }
    let args: Vec<&str> = plan_args.iter().map(String::as_str).collect();
    let plan = match t.command(&work, &args).output() {
        Ok(o) => o,
        Err(e) => return fail(report, "plan", Vec::new(), format!("failed to run plan: {e}")),
    };
    let stream = String::from_utf8_lossy(&plan.stdout).to_string();
    let diags = parse_stream(&stream, a);
    if !plan.status.success() {
        let raw = String::from_utf8_lossy(&plan.stderr).to_string();
        return fail(report, "plan", diags, raw);
    }
    let show = match t
        .command(&work, &["show", "-json", "-no-color", "tfplan"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => return fail(report, "show", Vec::new(), output_text(&o)),
        Err(e) => return fail(report, "show", Vec::new(), format!("failed to run show: {e}")),
    };
    match parse_plan(&String::from_utf8_lossy(&show.stdout), a) {
        Ok((entities, unattributed)) => {
            report.status = PlanStatus::Planned;
            for e in &entities {
                report.create += e.create;
                report.update += e.update;
                report.delete += e.delete;
                report.replace += e.replace;
            }
            for u in &unattributed {
                match u.change.as_str() {
                    "create" => report.create += 1,
                    "update" => report.update += 1,
                    "delete" => report.delete += 1,
                    "replace" => report.replace += 1,
                    _ => {}
                }
            }
            report.summary = format!(
                "{} to create, {} to change, {} to destroy{} across {} entities",
                report.create,
                report.update,
                report.delete,
                if report.replace > 0 {
                    format!(", {} to replace", report.replace)
                } else {
                    String::new()
                },
                entities.len()
            );
            report.entities = entities;
            report.unattributed = unattributed;
            report.diagnostics = diags;
        }
        Err(e) => {
            report.summary = e;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribution() -> Attribution {
        let mut a = Attribution::default();
        a.addresses.insert("aws_db_instance.db".into(), "db-1".into());
        a.addresses
            .insert("aws_db_subnet_group.db_subnets".into(), "db-1".into());
        a.addresses.insert("aws_nat_gateway.nat".into(), "nat-1".into());
        a.addresses
            .insert("data.aws_iam_policy_document.role_trust".into(), "role-1".into());
        a.sections.push(("database.tf".into(), 4, 40, "db-1".into()));
        a.names.insert("db-1".into(), "db".into());
        a.names.insert("nat-1".into(), "nat".into());
        a.definitions.insert(
            "db-1".into(),
            "definitions/resources/relational_database.toml".into(),
        );
        a
    }

    /// A recorded `show -json`, trimmed to what is read.
    const PLAN: &str = r#"{
      "format_version": "1.2",
      "resource_changes": [
        {"address": "aws_db_subnet_group.db_subnets", "change": {"actions": ["create"]}},
        {"address": "aws_db_instance.db", "change": {"actions": ["create"]}},
        {"address": "aws_nat_gateway.nat[0]", "change": {"actions": ["delete", "create"]}},
        {"address": "data.aws_iam_policy_document.role_trust", "change": {"actions": ["read"]}},
        {"address": "aws_s3_bucket.handmade", "change": {"actions": ["update"]}}
      ]
    }"#;

    #[test]
    fn a_plan_is_grouped_by_entity() {
        let (by, rest) = parse_plan(PLAN, &attribution()).unwrap();
        let db = by.iter().find(|e| e.entity == "db-1").unwrap();
        assert_eq!((db.create, db.name.as_str(), db.addresses.len()), (2, "db", 2));
        assert_eq!(db.badge(), Some("+"));
        let nat = by.iter().find(|e| e.entity == "nat-1").unwrap();
        assert_eq!(nat.replace, 1, "an index is dropped to find the entity");
        assert_eq!(nat.badge(), Some("\u{b1}"));
        assert_eq!(by.iter().find(|e| e.entity == "role-1").unwrap().read, 1);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].change, "update");
    }

    /// A recorded `validate -json`, trimmed.
    const VALIDATE: &str = r#"{"format_version":"1.0","valid":false,"error_count":2,"warning_count":0,
      "diagnostics":[
        {"severity":"error","summary":"Unsupported argument","detail":"An argument named \"multi_azz\" is not expected here.",
         "range":{"filename":"database.tf","start":{"line":12,"column":3},"end":{"line":12,"column":12}}},
        {"severity":"error","summary":"Missing required argument","detail":"x",
         "range":{"filename":"variables.tf","start":{"line":3,"column":1},"end":{"line":3,"column":2}}}
      ]}"#;

    #[test]
    fn validate_errors_find_their_entity_by_file_and_line() {
        let (valid, ds) = parse_validate(VALIDATE, &attribution()).unwrap();
        assert!(!valid);
        assert_eq!(ds[0].entity.as_deref(), Some("db-1"));
        assert_eq!(ds[0].entity_name.as_deref(), Some("db"));
        assert_eq!(
            ds[0].definition.as_deref(),
            Some("definitions/resources/relational_database.toml")
        );
        assert_eq!(ds[1].entity, None, "variables.tf belongs to no entity");
    }

    #[test]
    fn a_plan_stream_says_when_credentials_are_missing() {
        let stream = r#"{"@level":"info","@message":"OpenTofu 1.10.0","type":"version"}
{"@level":"error","@message":"Error: No valid credential sources found","type":"diagnostic","diagnostic":{"severity":"error","summary":"No valid credential sources found","detail":"Please see https://registry.terraform.io/providers/hashicorp/aws","range":{"filename":"providers.tf","start":{"line":4,"column":1}}}}
{"@level":"error","@message":"Error: Unsupported argument","type":"diagnostic","diagnostic":{"severity":"error","summary":"Unsupported argument","detail":"","address":"aws_db_instance.db"}}"#;
        let ds = parse_stream(stream, &attribution());
        assert_eq!(ds.len(), 2);
        assert!(is_credentials_error(&ds[0]));
        assert!(!is_credentials_error(&ds[1]));
        assert_eq!(ds[1].entity.as_deref(), Some("db-1"), "attributed by address");
    }
}
