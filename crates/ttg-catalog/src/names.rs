//! Which arguments and fields hold the name of what a resource creates, read from the
//! mappings rather than listed by hand.
//!
//! An argument is a *name argument* of a provider resource type (`aws_lb.name`,
//! `aws_s3_bucket.bucket`) when some mapping fills it from the entity's name (`field =
//! "name"`, a `{name}` template, through `if` branches and fallbacks) — the arguments the
//! name prefix reaches — or from a field marked `unique_scope`; arguments that only
//! describe it (`description`, `comment`, `display_name`) and `terraform_data`, which
//! creates nothing, are left out. A *name field* is a field whose value a mapping writes
//! into a name argument: a provider field such as the bucket's `bucket_name`, or the rows
//! of a table field a repeated block names its instances after (the registry's
//! `repositories`). What a project sets by hand in either is a name the prefix does not
//! reach.

use crate::schema::{ArgSource, FieldDef, ProviderMapping};
use crate::Catalog;
use std::collections::{BTreeMap, BTreeSet};

/// A field of one entity type that holds a provider-facing name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameField {
    /// The field, as in the entity's `config` or `provider_config`.
    pub field: String,
    /// It is a field of the provider mapping, not an abstract one.
    pub provider: bool,
    /// For a table field, the row item that holds the name (`value` for a string list).
    pub item: Option<String>,
    pub label: String,
}

/// Text about a resource that a mapping writes its name into, not a name.
fn describes(resource: &str, arg: &str) -> bool {
    resource == "terraform_data"
        || arg.contains("description")
        || arg.contains("comment")
        || arg == "display_name"
}

/// Does this source write the entity's own name?
fn names_entity(src: &ArgSource) -> bool {
    match src {
        ArgSource::Field(f) => f.field == "name" || f.fallback.as_deref().is_some_and(names_entity),
        ArgSource::ProviderField(f) => f.fallback.as_deref().is_some_and(names_entity),
        ArgSource::Template(t) => t.template.contains("{name}"),
        ArgSource::If(i) => names_entity(&i.then) || i.otherwise.as_deref().is_some_and(names_entity),
        _ => false,
    }
}

/// The fields a source writes as they are (through `if` branches and fallbacks):
/// `(field, provider field?, row item?)`. `item` sources count only inside a block
/// repeated over `rows`.
fn written_fields(src: &ArgSource, rows: Option<&str>, out: &mut Vec<(String, bool, Option<String>)>) {
    match src {
        ArgSource::Field(f) => {
            if f.field != "name" {
                out.push((f.field.clone(), false, f.column.clone()));
            }
            if let Some(fb) = &f.fallback {
                written_fields(fb, rows, out);
            }
        }
        ArgSource::ProviderField(f) => {
            out.push((f.provider_field.clone(), true, f.column.clone()));
            if let Some(fb) = &f.fallback {
                written_fields(fb, rows, out);
            }
        }
        ArgSource::Item(i) => {
            if let Some(rows) = rows {
                out.push((rows.to_string(), false, Some(i.item.clone())));
            }
        }
        ArgSource::If(i) => {
            written_fields(&i.then, rows, out);
            if let Some(o) = &i.otherwise {
                written_fields(o, rows, out);
            }
        }
        _ => {}
    }
}

impl Catalog {
    /// Every provider resource type's name arguments (see the module docs).
    pub fn name_arguments(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for def in self.resources.values() {
            for m in def.providers.values() {
                let unique: BTreeSet<(&str, bool)> = def
                    .fields
                    .iter()
                    .map(|f| (f, false))
                    .chain(m.fields.iter().map(|f| (f, true)))
                    .filter(|(f, _)| f.unique_scope.is_some())
                    .map(|(f, p)| (f.name.as_str(), p))
                    .collect();
                for b in &m.blocks {
                    for (arg, src) in b.args.iter().filter(|(a, _)| !describes(&b.resource, a)) {
                        let mut fields = Vec::new();
                        written_fields(src, None, &mut fields);
                        let from_unique = fields.iter().any(|(f, p, _)| unique.contains(&(f.as_str(), *p)));
                        if names_entity(src) || from_unique {
                            out.entry(b.resource.clone()).or_default().insert(arg.clone());
                        }
                    }
                }
            }
        }
        out
    }

    /// The name fields of `type_id` on `provider` (see the module docs), given
    /// [`Catalog::name_arguments`].
    pub fn name_fields(
        &self,
        type_id: &str,
        provider: &str,
        name_args: &BTreeMap<String, BTreeSet<String>>,
    ) -> Vec<NameField> {
        let (Some(def), Some(m)) = (self.resource(type_id), self.mapping(type_id, provider)) else {
            return Vec::new();
        };
        let find = |name: &str, provider: bool| -> Option<&FieldDef> {
            let fields = if provider { &m.fields } else { &def.fields };
            fields.iter().find(|f| f.name == name)
        };
        let mut out: Vec<NameField> = Vec::new();
        let mut push = |field: &str, provider: bool, item: Option<String>| {
            let Some(f) = find(field, provider) else { return };
            let nf = NameField {
                field: field.to_string(),
                provider,
                item,
                label: f.label().to_string(),
            };
            if !out.contains(&nf) {
                out.push(nf);
            }
        };
        for f in def.fields.iter().filter(|f| f.unique_scope.is_some()) {
            push(&f.name, false, None);
        }
        for f in m.fields.iter().filter(|f| f.unique_scope.is_some()) {
            push(&f.name, true, None);
        }
        for (field, provider, item) in mapping_name_fields(m, name_args) {
            push(&field, provider, item);
        }
        out
    }
}

/// The fields a mapping writes into a name argument.
fn mapping_name_fields(
    m: &ProviderMapping,
    name_args: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<(String, bool, Option<String>)> {
    let mut out = Vec::new();
    for b in &m.blocks {
        let Some(names) = name_args.get(&b.resource) else {
            continue;
        };
        for src in b.args.iter().filter_map(|(a, s)| names.contains(a).then_some(s)) {
            written_fields(src, b.for_each_field.as_deref(), &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_mappings() {
        let cat = Catalog::builtin();
        let args = cat.name_arguments();
        let has = |res: &str, arg: &str| args.get(res).is_some_and(|a| a.contains(arg));
        assert!(has("aws_lb", "name") && has("aws_lb_target_group", "name"));
        assert!(has("aws_db_instance", "identifier") && has("aws_secretsmanager_secret", "name"));
        // A bucket's name is a field of its own, marked unique.
        assert!(has("aws_s3_bucket", "bucket"));
        // What only describes the resource is not a name.
        assert!(!has("aws_cloudwatch_metric_alarm", "alarm_description"));
        assert!(!args.contains_key("terraform_data"));
        let fields = |ty: &str| cat.name_fields(ty, "aws", &args);
        assert!(fields("object_storage")
            .iter()
            .any(|f| f.field == "bucket_name" && f.provider && f.label == "Bucket name"));
        assert!(fields("container_registry")
            .iter()
            .any(|f| f.field == "repositories" && !f.provider && f.item.as_deref() == Some("value")));
        assert!(fields("load_balancer").is_empty(), "named after the entity only");
    }
}
