//! Named environments: one diagram, the values that differ between `staging` and `prod`.
//!
//! `settings.environments` lists the environments in order. What may differ between them:
//!
//! - **field values**: a node or container carries `overrides`, environment name → the
//!   `config` / `provider_config` values that replace its own there;
//! - **presence**: an override may say the entity is `absent` from that environment (a
//!   second NAT gateway only prod has), and a link may be tagged with the
//!   `environments` it belongs to;
//! - **project variables** (`settings.variables`): a value declared once, with a value per
//!   environment, used in any field (and in `extra` arguments) as `${var.<name>}`. Two
//!   are built in: `${var.environment}`, the environment's name, and `${var.name_prefix}`,
//!   the rendered `settings.name_prefix` that is put in front of every generated resource
//!   name (`zipos-${var.environment}` → `zipos-staging-db`).
//!
//! [`Project::for_environment`] resolves all of that for one environment and returns an
//! ordinary project: overrides folded in, absent entities and links gone, variables
//! substituted, the name prefix rendered. Everything downstream — diagnostics,
//! reachability, the cost estimate, code generation — works on that resolved project, so
//! none of it needs to know environments exist. The code generator is the one consumer
//! that looks at every environment at once: it resolves each, generates each and turns
//! what differs into variables (`ttg_codegen::environments`).

use crate::ir::{Config, Id, Project, ProviderId};
use crate::Value;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What one environment changes on one entity. A value here replaces the entity's own
/// value for that field in that environment; a field without one keeps the base value.
/// An override cannot remove a value, only replace it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnvOverride {
    /// Abstract field values, as in the entity's `config`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: Config,
    /// Provider field values, as in the entity's `provider_config`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_config: BTreeMap<ProviderId, Config>,
    /// The entity does not exist in this environment: it, and every link to and from it,
    /// is left out there (a container's contents move up to its parent).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
}

impl EnvOverride {
    pub fn is_empty(&self) -> bool {
        !self.absent && self.config.is_empty() && self.provider_config.values().all(|c| c.is_empty())
    }

    /// Drop empty provider maps so files stay tidy.
    pub fn prune(&mut self) {
        self.provider_config.retain(|_, c| !c.is_empty());
    }
}

/// Per-environment overrides of one entity, keyed by environment name.
pub type Overrides = BTreeMap<String, EnvOverride>;

/// A project variable: declared once in the settings, used in field values as
/// `${var.<name>}`, given a value per environment. Where the values differ the export
/// declares it as a Terraform variable of the same name, set in each environment's
/// `.tfvars`; where they do not, the value is written in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectVariable {
    /// The value outside any environment, and in every environment without its own.
    pub value: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Environment → its value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, Value>,
}

/// The project variables every project has: the environment's name and the rendered
/// name prefix. A project variable may not use these names.
pub const BUILTIN_VARIABLES: [&str; 2] = ["environment", "name_prefix"];

/// Whether `name` can be an environment: it names a `.tfvars` file and a segment of the
/// state key, so lowercase letters, digits, `-` and `_`, starting with a letter.
pub fn check_environment_name(name: &str) -> Result<(), String> {
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok && name.len() <= 32 {
        Ok(())
    } else {
        Err(format!(
            "environment name \"{name}\" must start with a lowercase letter and use only a-z, 0-9, - and _ (at most 32 characters): it names a .tfvars file and part of the state key"
        ))
    }
}

/// Whether `name` can be a project variable: it becomes a Terraform variable name.
pub fn check_variable_name(name: &str) -> Result<(), String> {
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err(format!(
            "variable name \"{name}\" must start with a lowercase letter and use only a-z, 0-9 and _"
        ));
    }
    if BUILTIN_VARIABLES.contains(&name) {
        return Err(format!("\"{name}\" is built in: ${{var.{name}}} is always there"));
    }
    Ok(())
}

/// The `${var.<name>}` references in a string, in order.
pub fn variable_refs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${var.") {
        let after = &rest[i + 6..];
        match after.find('}') {
            Some(j) => {
                out.push(&after[..j]);
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

/// Substitute `${var.<name>}` in a string. A string that is nothing but one reference
/// takes the variable's value as it is (a number stays a number); otherwise each known
/// reference is replaced by the value's text. Unknown references are left in place (the
/// diagnostics name them).
pub fn substitute(s: &str, vars: &BTreeMap<String, Value>) -> Value {
    if let Some(name) = s.strip_prefix("${var.").and_then(|r| r.strip_suffix('}')) {
        if !name.contains('}') {
            if let Some(v) = vars.get(name) {
                return v.clone();
            }
        }
    }
    let mut out = s.to_string();
    for name in variable_refs(s) {
        if let Some(v) = vars.get(name) {
            out = out.replace(&format!("${{var.{name}}}"), &v.display());
        }
    }
    Value::Str(out)
}

fn substitute_value(v: &mut Value, vars: &BTreeMap<String, Value>) {
    match v {
        Value::Str(s) if s.contains("${var.") => *v = substitute(s, vars),
        Value::List(items) => {
            for s in items.iter_mut().filter(|s| s.contains("${var.")) {
                *s = substitute(s, vars).display();
            }
        }
        Value::Records(rows) => {
            for r in rows {
                for x in r.values_mut() {
                    substitute_value(x, vars);
                }
            }
        }
        _ => {}
    }
}

/// Substitute in an `extra` argument tree. A `{"$raw": …}` is HCL, where `${var.x}` is
/// Terraform's own interpolation, so it is left alone.
fn substitute_json(v: &mut serde_json::Value, vars: &BTreeMap<String, Value>) {
    match v {
        serde_json::Value::String(s) if s.contains("${var.") => {
            *v = serde_json::to_value(substitute(s, vars)).unwrap_or(serde_json::Value::Null);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|x| substitute_json(x, vars)),
        serde_json::Value::Object(o) if !o.contains_key("$raw") => {
            o.values_mut().for_each(|x| substitute_json(x, vars))
        }
        _ => {}
    }
}

fn apply(o: &EnvOverride, config: &mut Config, provider_config: &mut BTreeMap<ProviderId, Config>) {
    for (k, v) in &o.config {
        config.insert(k.clone(), v.clone());
    }
    for (pid, vals) in &o.provider_config {
        let m = provider_config.entry(pid.clone()).or_default();
        for (k, v) in vals {
            m.insert(k.clone(), v.clone());
        }
    }
}

impl Project {
    /// The project as it is in environment `env` (see the module docs): its overrides
    /// folded into `config` / `provider_config`, the entities absent there removed with
    /// their links (a removed container's contents move up), links tagged for other
    /// environments removed, project variables substituted in every field and `extra`
    /// argument, `${var.…}` in the tags and the name prefix rendered. Overrides and
    /// variables are cleared — the result describes one environment and nothing else —
    /// but the list of environments is kept, so an export of it still keeps one state per
    /// environment. An environment nothing overrides gives the base values.
    ///
    /// Diagnostics, reachability, the cost estimate and anything else that answers "what
    /// is deployed in prod" should be run on this, not on the project itself.
    pub fn for_environment(&self, env: &str) -> Project {
        self.resolve_with(Some(env))
    }

    /// [`Project::for_environment`] when an environment is chosen. With none, the base
    /// values: overrides ignored, but project variables still substituted with their base
    /// values (`${var.environment}` is empty there) — borrowed unchanged when the project
    /// uses no variables at all.
    pub fn resolved(&self, env: Option<&str>) -> std::borrow::Cow<'_, Project> {
        match env {
            Some(e) => std::borrow::Cow::Owned(self.for_environment(e)),
            None if self.uses_variables() => std::borrow::Cow::Owned(self.resolve_with(None)),
            None => std::borrow::Cow::Borrowed(self),
        }
    }

    /// Does anything in the project need substituting (a `${var.…}` anywhere)?
    pub fn uses_variables(&self) -> bool {
        fn in_value(v: &Value) -> bool {
            match v {
                Value::Str(s) => s.contains("${var."),
                Value::List(l) => l.iter().any(|s| s.contains("${var.")),
                Value::Records(rows) => rows.iter().any(|r| r.values().any(in_value)),
                _ => false,
            }
        }
        let s = &self.settings;
        s.name_prefix.is_some()
            || s.tags.values().any(|t| t.contains("${var."))
            || self.entities().iter().any(|e| {
                e.config.values().any(in_value)
                    || e.provider_config.values().any(|c| c.values().any(in_value))
                    || serde_json::to_string(e.extra).is_ok_and(|t| t.contains("${var."))
            })
    }

    /// Every project variable's value in `env` (the base value with `None`), the two
    /// built-in ones included: `environment` (empty with `None`) and `name_prefix`.
    pub fn variable_values(&self, env: Option<&str>) -> BTreeMap<String, Value> {
        let mut out: BTreeMap<String, Value> = self
            .settings
            .variables
            .iter()
            .map(|(k, v)| {
                let val = env
                    .and_then(|e| v.environments.get(e))
                    .unwrap_or(&v.value)
                    .clone();
                (k.clone(), val)
            })
            .collect();
        out.insert(
            "environment".into(),
            Value::Str(env.unwrap_or_default().to_string()),
        );
        let prefix = self
            .settings
            .name_prefix
            .as_deref()
            .map(|p| substitute(p, &out).display())
            .unwrap_or_default();
        // Outside any environment `zipos-${var.environment}` is `zipos-`, not `zipos--db`.
        let mut prefix = prefix.trim().trim_matches('-').to_string();
        while prefix.contains("--") {
            prefix = prefix.replace("--", "-");
        }
        out.insert("name_prefix".into(), Value::Str(prefix));
        out
    }

    /// The name a resource is created with: the display name behind the rendered
    /// `name_prefix`, if there is one. Mappings that build a provider name from the
    /// entity's name (`field = "name"`, `{name}`) use this; the HCL local name does not.
    pub fn resource_name(&self, name: &str) -> String {
        match self.settings.name_prefix.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() && !p.contains("${var.") => format!("{p}-{name}"),
            _ => name.to_string(),
        }
    }

    fn resolve_with(&self, env: Option<&str>) -> Project {
        let vars = self.variable_values(env);
        let mut out = self.clone();
        if let Some(env) = env {
            let absent: BTreeSet<Id> = self
                .entities()
                .iter()
                .filter(|e| {
                    self.overrides_of(e.id)
                        .and_then(|o| o.get(env))
                        .is_some_and(|o| o.absent)
                })
                .map(|e| e.id.to_string())
                .collect();
            for id in &absent {
                out.remove_entity(id);
            }
            out.edges
                .retain(|e| e.environments.is_empty() || e.environments.iter().any(|x| x == env));
            for n in out.nodes.values_mut() {
                if let Some(o) = n.overrides.get(env).cloned() {
                    apply(&o, &mut n.config, &mut n.provider_config);
                }
            }
            for c in out.containers.values_mut() {
                if let Some(o) = c.overrides.get(env).cloned() {
                    apply(&o, &mut c.config, &mut c.provider_config);
                }
            }
        }
        let sub_config = |config: &mut Config, provider_config: &mut BTreeMap<ProviderId, Config>| {
            config.values_mut().for_each(|v| substitute_value(v, &vars));
            for c in provider_config.values_mut() {
                c.values_mut().for_each(|v| substitute_value(v, &vars));
            }
        };
        for n in out.nodes.values_mut() {
            n.overrides.clear();
            sub_config(&mut n.config, &mut n.provider_config);
            for blocks in n.extra.values_mut() {
                for args in blocks.values_mut() {
                    args.values_mut().for_each(|v| substitute_json(v, &vars));
                }
            }
        }
        for c in out.containers.values_mut() {
            c.overrides.clear();
            sub_config(&mut c.config, &mut c.provider_config);
            for blocks in c.extra.values_mut() {
                for args in blocks.values_mut() {
                    args.values_mut().for_each(|v| substitute_json(v, &vars));
                }
            }
        }
        for t in out.settings.tags.values_mut() {
            if t.contains("${var.") {
                *t = substitute(t, &vars).display();
            }
        }
        let prefix = vars.get("name_prefix").map(|v| v.display()).unwrap_or_default();
        out.settings.name_prefix = (!prefix.is_empty()).then_some(prefix);
        out.settings.variables.clear();
        out
    }

    /// The overrides an entity carries (empty for an unknown id).
    pub fn overrides_of(&self, id: &str) -> Option<&Overrides> {
        if let Some(n) = self.nodes.get(id) {
            return Some(&n.overrides);
        }
        self.containers.get(id).map(|c| &c.overrides)
    }

    /// Mutable overrides of an entity.
    pub fn overrides_mut(&mut self, id: &str) -> Option<&mut Overrides> {
        if let Some(n) = self.nodes.get_mut(id) {
            return Some(&mut n.overrides);
        }
        self.containers.get_mut(id).map(|c| &mut c.overrides)
    }

    /// Drop empty override entries of one entity.
    pub fn prune_overrides(&mut self, id: &str) {
        if let Some(o) = self.overrides_mut(id) {
            for e in o.values_mut() {
                e.prune();
            }
            o.retain(|_, e| !e.is_empty());
        }
    }

    /// Is the entity absent from `env`?
    pub fn absent_in(&self, id: &str, env: &str) -> bool {
        self.overrides_of(id)
            .and_then(|o| o.get(env))
            .is_some_and(|o| o.absent)
    }

    /// Does any entity override anything in `env`?
    pub fn has_overrides_for(&self, env: &str) -> bool {
        self.nodes
            .values()
            .map(|n| &n.overrides)
            .chain(self.containers.values().map(|c| &c.overrides))
            .any(|o| o.get(env).is_some_and(|e| !e.is_empty()))
    }

    /// Replace the list of environments. Overrides, link tags and variable values for an
    /// environment that is no longer listed go with it; the ids of the entities that lost
    /// overrides are returned, so a caller can say what went.
    pub fn set_environments(&mut self, envs: Vec<String>) -> Result<Vec<Id>, String> {
        let mut seen = BTreeSet::new();
        for e in &envs {
            check_environment_name(e)?;
            if !seen.insert(e.clone()) {
                return Err(format!("environment \"{e}\" is listed twice"));
            }
        }
        let mut lost = Vec::new();
        let mut prune = |id: &Id, o: &mut Overrides| {
            let before = o.len();
            o.retain(|k, _| envs.contains(k));
            if o.len() != before {
                lost.push(id.clone());
            }
        };
        for n in self.nodes.values_mut() {
            prune(&n.id, &mut n.overrides);
        }
        for c in self.containers.values_mut() {
            prune(&c.id, &mut c.overrides);
        }
        for e in &mut self.edges {
            e.environments.retain(|x| envs.contains(x));
        }
        for v in self.settings.variables.values_mut() {
            v.environments.retain(|k, _| envs.contains(k));
        }
        self.settings.environments = envs;
        Ok(lost)
    }

    /// Rename an environment, carrying every override, link tag and variable value with it.
    pub fn rename_environment(&mut self, from: &str, to: &str) -> Result<(), String> {
        check_environment_name(to)?;
        if from == to {
            return Ok(());
        }
        if self.settings.environments.iter().any(|e| e == to) {
            return Err(format!("there is already an environment \"{to}\""));
        }
        let Some(slot) = self.settings.environments.iter_mut().find(|e| e.as_str() == from) else {
            return Err(format!("no environment \"{from}\""));
        };
        *slot = to.to_string();
        let mv = |o: &mut Overrides| {
            if let Some(v) = o.remove(from) {
                o.insert(to.to_string(), v);
            }
        };
        self.nodes.values_mut().for_each(|n| mv(&mut n.overrides));
        self.containers.values_mut().for_each(|c| mv(&mut c.overrides));
        for e in &mut self.edges {
            for x in e.environments.iter_mut().filter(|x| x.as_str() == from) {
                *x = to.to_string();
            }
        }
        for v in self.settings.variables.values_mut() {
            if let Some(x) = v.environments.remove(from) {
                v.environments.insert(to.to_string(), x);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Node;
    use crate::Value;

    fn node(id: &str, ty: &str) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            resource_type: ty.into(),
            config: Default::default(),
            provider_config: Default::default(),
            position: Default::default(),
            size: None,
            parent: None,
            manual: false,
            providers: Vec::new(),
            extra: Default::default(),
            classification: None,
            description: String::new(),
            owner: String::new(),
            overrides: Default::default(),
        }
    }

    fn project() -> Project {
        let mut p = Project::new("t");
        let mut n = node("db", "relational_database");
        n.config.insert("high_availability".into(), Value::Bool(false));
        n.provider_config.insert(
            "aws".into(),
            [("instance_class".to_string(), Value::Str("db.t4g.small".into()))].into(),
        );
        n.overrides.insert(
            "prod".into(),
            EnvOverride {
                config: [("high_availability".to_string(), Value::Bool(true))].into(),
                provider_config: [(
                    "aws".to_string(),
                    [("instance_class".to_string(), Value::Str("db.r6g.large".into()))].into(),
                )]
                .into(),
                absent: false,
            },
        );
        p.nodes.insert("db".into(), n);
        p.settings.environments = vec!["pilot".into(), "prod".into()];
        p
    }

    #[test]
    fn an_environment_folds_its_overrides_in() {
        let p = project();
        let prod = p.for_environment("prod");
        let db = &prod.nodes["db"];
        assert_eq!(db.config["high_availability"], Value::Bool(true));
        assert_eq!(
            db.provider_config["aws"]["instance_class"],
            Value::Str("db.r6g.large".into())
        );
        assert!(
            db.overrides.is_empty(),
            "the resolved project describes one environment"
        );
        assert_eq!(prod.settings.environments, p.settings.environments);
        // An environment without overrides is the base.
        let pilot = p.for_environment("pilot");
        assert_eq!(pilot.nodes["db"].config["high_availability"], Value::Bool(false));
        assert!(p.has_overrides_for("prod") && !p.has_overrides_for("pilot"));
    }

    #[test]
    fn absent_entities_and_tagged_links_leave_the_environment() {
        let mut p = project();
        p.nodes.insert("nat-b".into(), node("nat-b", "nat_gateway"));
        p.nodes.insert("rt".into(), node("rt", "route_table"));
        p.overrides_mut("nat-b").unwrap().insert(
            "pilot".into(),
            EnvOverride {
                absent: true,
                ..Default::default()
            },
        );
        p.add_edge("rt", "nat-b", crate::Relation::AttributeReference);
        p.add_edge("rt", "db", crate::Relation::DependsOn);
        p.edges[1].environments = vec!["prod".into()];
        let pilot = p.for_environment("pilot");
        assert!(!pilot.nodes.contains_key("nat-b") && pilot.edges.is_empty());
        let prod = p.for_environment("prod");
        assert!(prod.nodes.contains_key("nat-b") && prod.edges.len() == 2);
        // The base has everything.
        assert_eq!(p.resolved(None).edges.len(), 2);
    }

    #[test]
    fn project_variables_and_the_name_prefix_are_substituted() {
        let mut p = project();
        p.settings.variables.insert(
            "db_class".into(),
            ProjectVariable {
                value: Value::Str("db.t4g.small".into()),
                description: String::new(),
                environments: [("prod".to_string(), Value::Str("db.r6g.xlarge".into()))].into(),
            },
        );
        p.settings.variables.insert(
            "tasks".into(),
            ProjectVariable {
                value: Value::Int(1),
                description: String::new(),
                environments: [("prod".to_string(), Value::Int(3))].into(),
            },
        );
        p.settings.name_prefix = Some("zipos-${var.environment}".into());
        p.settings
            .tags
            .insert("Environment".into(), "${var.environment}".into());
        let db = p.nodes.get_mut("db").unwrap();
        db.overrides.clear();
        db.provider_config
            .get_mut("aws")
            .unwrap()
            .insert("instance_class".into(), Value::Str("${var.db_class}".into()));
        db.config
            .insert("replicas".into(), Value::Str("${var.tasks}".into()));
        db.config
            .insert("label".into(), Value::Str("db-${var.environment}".into()));
        let prod = p.for_environment("prod");
        let db = &prod.nodes["db"];
        assert_eq!(
            db.provider_config["aws"]["instance_class"],
            Value::Str("db.r6g.xlarge".into())
        );
        assert_eq!(
            db.config["replicas"],
            Value::Int(3),
            "a whole reference keeps its type"
        );
        assert_eq!(db.config["label"], Value::Str("db-prod".into()));
        assert_eq!(prod.settings.tags["Environment"], "prod");
        assert_eq!(prod.settings.name_prefix.as_deref(), Some("zipos-prod"));
        assert_eq!(prod.resource_name("db"), "zipos-prod-db");
        assert!(prod.settings.variables.is_empty());
        let base = p.resolved(None);
        assert_eq!(base.nodes["db"].config["replicas"], Value::Int(1));
        assert_eq!(base.resource_name("db"), "zipos-db");
    }

    #[test]
    fn renaming_and_removing_environments_carries_or_drops_overrides() {
        let mut p = project();
        p.rename_environment("prod", "production").unwrap();
        assert!(p.nodes["db"].overrides.contains_key("production"));
        assert_eq!(p.settings.environments, vec!["pilot", "production"]);
        assert!(p.rename_environment("pilot", "production").is_err());
        assert!(
            p.rename_environment("pilot", "Pilot").is_err(),
            "names are lowercase"
        );
        let lost = p.set_environments(vec!["pilot".into()]).unwrap();
        assert_eq!(lost, vec!["db".to_string()]);
        assert!(p.nodes["db"].overrides.is_empty());
        assert!(p.set_environments(vec!["a".into(), "a".into()]).is_err());
        assert!(check_variable_name("environment").is_err() && check_variable_name("db_class").is_ok());
    }

    #[test]
    fn a_file_without_environments_still_loads_and_saves_unchanged() {
        let json =
            r#"{"schema_version":1,"name":"x","nodes":{"n":{"id":"n","name":"n","resource_type":"subnet"}}}"#;
        let p: Project = serde_json::from_str(json).unwrap();
        assert!(p.settings.environments.is_empty() && p.nodes["n"].overrides.is_empty());
        let out = serde_json::to_string(&p).unwrap();
        assert!(!out.contains("environments") && !out.contains("overrides"));
        assert!(!p.uses_variables());
    }
}
