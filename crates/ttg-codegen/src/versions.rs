//! Provider versions: which version constraint goes into `required_providers`, and
//! whether the bundled provider schema (`ttg-schema`) describes the versions it admits.
//!
//! A project may pin a provider (`settings.provider_versions`, e.g. `{ "aws": "~> 6.0" }`);
//! without an entry the provider definition's `version_constraint` is the default. The
//! argument checks — extra arguments on the canvas, and here the curated mappings the
//! project uses — run against one schema per provider, the one bundled (or refreshed with
//! `ttg schema refresh`). When the project's constraint cannot select that schema's major
//! version the checks would be answering for a different provider than `init` installs,
//! so the diagnostics say so instead of staying silent.

use crate::diagnostics::{Code, Diagnostic, Severity};
use ttg_catalog::{BlockDef, Catalog, NestedBlockDef, ProviderDef};
use ttg_core::Project;
use ttg_schema::BlockSchema;

/// The version constraint the export writes for a provider.
pub fn constraint<'a>(p: &'a Project, pdef: &'a ProviderDef) -> &'a str {
    p.settings
        .provider_versions
        .get(&pdef.provider.id)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(&pdef.provider.version_constraint)
}

/// One clause of a constraint: operator and numeric version parts.
struct Clause {
    op: &'static str,
    parts: Vec<u64>,
}

fn parse(s: &str) -> Result<Vec<Clause>, String> {
    let mut out = Vec::new();
    for raw in s.split(',') {
        let c = raw.trim();
        if c.is_empty() {
            return Err(format!("\"{s}\" has an empty clause"));
        }
        let (op, rest) = ["~>", ">=", "<=", "!=", ">", "<", "="]
            .iter()
            .find_map(|op| c.strip_prefix(op).map(|r| (*op, r.trim())))
            .unwrap_or(("=", c));
        let version = rest.split(['-', '+']).next().unwrap_or("");
        let parts: Result<Vec<u64>, _> = version.split('.').map(|n| n.parse::<u64>()).collect();
        match parts {
            Ok(parts) if (1..=3).contains(&parts.len()) => out.push(Clause { op, parts }),
            _ => {
                return Err(format!(
                    "\"{c}\" is not a version constraint (expected e.g. \"~> 6.0\", \">= 5.0, < 7.0\")"
                ))
            }
        }
    }
    Ok(out)
}

/// Is `s` a version constraint `required_providers` accepts?
pub fn check_constraint(s: &str) -> Result<(), String> {
    parse(s).map(|_| ())
}

/// Can `constraint` select some version with this major number? `None` when the
/// constraint does not parse.
pub fn admits_major(constraint: &str, major: u64) -> Option<bool> {
    let clauses = parse(constraint).ok()?;
    let (mut lo, mut hi) = (0u64, u64::MAX);
    for c in &clauses {
        let x = c.parts[0];
        let rest_zero = c.parts[1..].iter().all(|n| *n == 0);
        match c.op {
            "=" => {
                lo = lo.max(x);
                hi = hi.min(x);
            }
            ">" | ">=" => lo = lo.max(x),
            "<=" => hi = hi.min(x),
            "<" if rest_zero => {
                if x == 0 {
                    return Some(false);
                }
                hi = hi.min(x - 1)
            }
            "<" => hi = hi.min(x),
            // `~> 6` lets every later major in; `~> 6.0` / `~> 6.1.2` stay on 6.
            "~>" if c.parts.len() == 1 => lo = lo.max(x),
            "~>" => {
                lo = lo.max(x);
                hi = hi.min(x);
            }
            _ => {} // `!=` excludes one version, never a whole major
        }
    }
    Some(lo <= major && major <= hi)
}

/// The major number of a version string (`6.66.0` -> 6).
fn major_of(v: &str) -> Option<u64> {
    v.trim().split('.').next()?.parse().ok()
}

/// Meta-arguments every resource accepts regardless of its schema.
const META: &[&str] = &["depends_on", "count", "for_each", "provider", "lifecycle"];

fn check_nested(schema: &BlockSchema, n: &NestedBlockDef, at: &str, errs: &mut Vec<String>) {
    let Some(ns) = schema.blocks.get(&n.block) else {
        errs.push(format!("{at}: nested block '{}' does not exist", n.block));
        return;
    };
    for k in n.args.keys() {
        let key = k.split('.').next().unwrap_or(k);
        if !ns.block().has(key) {
            errs.push(format!("{at}.{}: argument '{key}' does not exist", n.block));
        }
    }
    for inner in &n.nested {
        check_nested(ns.block(), inner, &format!("{at}.{}", n.block), errs);
    }
}

fn check_block(cat: &Catalog, provider: &str, b: &BlockDef, at: &str, errs: &mut Vec<String>) {
    if b.resource == "terraform_data" {
        return; // built into Terraform / OpenTofu, not part of any provider schema
    }
    // Helper providers (hashicorp/random) are not part of the bundled index.
    if cat.provider(provider).is_some_and(|p| {
        p.helper_providers
            .iter()
            .any(|h| b.resource.starts_with(&h.prefix))
    }) {
        return;
    }
    let idx = ttg_schema::index();
    let Some(schema) = idx.resource(provider, &b.resource) else {
        errs.push(format!(
            "{at}: resource type '{}' does not exist on {provider}",
            b.resource
        ));
        return;
    };
    for k in b.args.keys() {
        let key = k.split('.').next().unwrap_or(k);
        if !schema.has(key) && !META.contains(&key) {
            errs.push(format!("{at} ({}): argument '{key}' does not exist", b.resource));
        }
    }
    for n in &b.nested {
        check_nested(schema, n, &format!("{at} ({})", b.resource), errs);
    }
}

/// Where one curated mapping disagrees with the provider schema in use: resource types
/// that do not exist, and static arguments / nested blocks the provider does not accept.
/// Each finding starts with `<type>/<provider>/<block key>`. The `schema_check` test runs
/// it over the whole catalog; the diagnostics run it over the types a project uses.
pub fn mapping_findings(cat: &Catalog, provider: &str, type_id: &str) -> Vec<String> {
    let mut errs = Vec::new();
    if let Some(m) = cat.mapping(type_id, provider) {
        for b in &m.blocks {
            check_block(
                cat,
                provider,
                b,
                &format!("{type_id}/{provider}/{}", b.key),
                &mut errs,
            );
        }
    }
    errs
}

/// The version diagnostics for one provider's layer: a malformed pin, a pin whose major
/// the bundled schema does not describe, and — when it does describe it — every curated
/// mapping the project uses that the schema says will not validate.
pub fn checks(layer: &Project, cat: &Catalog, provider: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(pdef) = cat.provider(provider) else {
        return out;
    };
    let push = |out: &mut Vec<Diagnostic>, entity: Option<&str>, severity, message: String| {
        out.push(Diagnostic {
            entity: entity.map(str::to_string),
            severity,
            code: Code::Version,
            message,
            provider: None,
        })
    };
    let pinned = constraint(layer, pdef);
    let name = &pdef.provider.source_name;
    if let Err(e) = check_constraint(pinned) {
        push(
            &mut out,
            None,
            Severity::Error,
            format!("provider version for {provider}: {e} (Settings ▸ Provider versions)"),
        );
        return out;
    }
    let idx = ttg_schema::index();
    let Some(schema) = idx.provider(provider) else {
        return out;
    };
    let Some(major) = major_of(&schema.version) else {
        return out;
    };
    if admits_major(pinned, major) == Some(false) {
        push(
            &mut out,
            None,
            Severity::Info,
            format!(
                "argument checks use {name} {major}.x (the {} schema, {} {}); your project pins {pinned}, so the arguments are not checked against the version `init` installs. Run `ttg schema refresh` with that version to check them",
                ttg_schema::index_source(),
                schema.source,
                schema.version
            ),
        );
        return out;
    }
    // The pin selects the schema's major: every mapping the layer emits must agree with it.
    let mut seen = std::collections::BTreeSet::new();
    for e in layer.entities() {
        if e.manual || ttg_catalog::load::Catalog::is_native(e.resource_type) {
            continue;
        }
        if !seen.insert(e.resource_type.to_string()) {
            continue;
        }
        let findings = mapping_findings(cat, provider, e.resource_type);
        if findings.is_empty() {
            continue;
        }
        push(
            &mut out,
            Some(e.id),
            Severity::Warning,
            format!(
                "the {provider} mapping of {} (definitions/resources/{}.toml) does not match {} {}, so the export will not validate: {}",
                e.resource_type,
                e.resource_type,
                schema.source,
                schema.version,
                findings.join("; ")
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majors_a_constraint_admits() {
        assert_eq!(admits_major("~> 6.0", 6), Some(true));
        assert_eq!(admits_major("~> 6.0", 5), Some(false));
        assert_eq!(admits_major("~> 5.0", 6), Some(false));
        assert_eq!(admits_major("~> 5.100.0", 5), Some(true));
        assert_eq!(admits_major("~> 5", 6), Some(true));
        assert_eq!(admits_major(">= 5.0, < 7.0", 6), Some(true));
        assert_eq!(admits_major(">= 5.0, < 6.0", 6), Some(false));
        assert_eq!(admits_major(">= 5.0, < 6.1", 6), Some(true));
        assert_eq!(admits_major("6.66.0", 6), Some(true));
        assert_eq!(admits_major("= 5.2.1", 6), Some(false));
        assert_eq!(admits_major("!= 6.1.0", 6), Some(true));
        assert_eq!(admits_major("banana", 6), None);
    }

    #[test]
    fn constraint_syntax() {
        assert!(check_constraint("~> 6.0").is_ok());
        assert!(check_constraint(">= 5.0, < 7.0").is_ok());
        assert!(check_constraint("6.0.0-beta1").is_ok());
        assert!(check_constraint("latest").is_err());
        assert!(check_constraint("~> 6.x").is_err());
        assert!(check_constraint(">= 5.0,").is_err());
    }
}
