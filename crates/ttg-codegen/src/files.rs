//! Rendering of the individual output files. All HCL goes through `hcl::format`; the
//! only string work here is comments, file headers and `terraform fmt`-style alignment.

use crate::emit::{ManualEntry, OutputSpec, VarSpec};
use crate::tool::Profile;
use hcl::{Block, Expression, Identifier, Object, ObjectKey};
use ttg_catalog::{Catalog, HelperProviderDef, ProviderDef};
use ttg_core::Project;

pub(crate) fn fmt_block(b: &Block) -> String {
    let s = hcl::format::to_string(b).expect("hcl formatting cannot fail for a built tree");
    let mut s = collapse_empty_braces(&align_attributes(&space_object_for(&s)));
    while s.ends_with("\n\n") {
        s.pop();
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// `{for o in x : k => o}` -> `{ for o in x : k => o }`: `fmt` pads an object `for`
/// expression inside its braces (a list one, `[for …]`, it leaves alone). Strings are
/// skipped, so a `{for` inside one is untouched.
fn space_object_for(s: &str) -> String {
    let b = s.as_bytes();
    // Byte offsets of the `{` opening an object `for` and of the `}` closing it.
    let mut opens: Vec<usize> = Vec::new();
    let mut closes: Vec<usize> = Vec::new();
    // Open braces outside strings: (offset, is an object `for`).
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            match c {
                b'\\' => i += 1,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => {
                    let is_for = s[i + 1..].starts_with("for ");
                    stack.push((i, is_for));
                }
                b'}' => {
                    if let Some((at, true)) = stack.pop() {
                        opens.push(at);
                        closes.push(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    if opens.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2 * opens.len());
    for (i, ch) in s.char_indices() {
        if closes.contains(&i) && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(ch);
        if opens.contains(&i) {
            out.push(' ');
        }
    }
    out
}

/// `none {     }` -> `none {}`. The formatter pads an empty body with the block's own
/// indentation, and WAF rules are full of deliberately empty blocks.
fn collapse_empty_braces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let spaces = after.len() - after.trim_start_matches(' ').len();
        if after[spaces..].starts_with('}') {
            out.push_str("{}");
            rest = &after[spaces + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// A `.tf` file of resource blocks, each entity introduced by a comment.
pub fn render_resource_file(header: &str, entries: &[(String, Vec<Block>)]) -> String {
    render_resource_file_with_lines(header, entries).0
}

/// [`render_resource_file`], plus the first and last line (1-based, inclusive) of each
/// entry: its `# ---` comment through the end of its last block. `validate -json` and
/// `plan -json` report errors by file and line, and this is how they find their entity.
pub fn render_resource_file_with_lines(
    header: &str,
    entries: &[(String, Vec<Block>)],
) -> (String, Vec<(usize, usize)>) {
    let mut out = String::from(header);
    let mut lines = Vec::with_capacity(entries.len());
    let newlines = |s: &str| s.bytes().filter(|b| *b == b'\n').count();
    for (comment, blocks) in entries {
        out.push('\n');
        let first = newlines(&out) + 1;
        out.push_str(&format!("# --- {comment}\n"));
        for b in blocks {
            out.push_str(&fmt_block(b));
            out.push('\n');
        }
        // Every block ends in a newline and is followed by a blank line; the last line
        // of the entry is the one before that blank.
        lines.push((first, newlines(&out) - 1));
    }
    (out, lines)
}

pub fn render_variables(header: &str, vars: &[&VarSpec]) -> String {
    let mut out = String::from(header);
    if vars.is_empty() {
        out.push_str("\n# No input variables are required for this configuration.\n");
        return out;
    }
    for v in vars {
        let mut b = Block::builder("variable")
            .add_label(v.name.as_str())
            .add_attribute(("type", raw(&v.var_type)));
        if !v.description.is_empty() {
            b = b.add_attribute(("description", v.description.as_str()));
        }
        if let Some(d) = &v.default {
            b = b.add_attribute(("default", d.clone()));
        }
        if v.sensitive {
            b = b.add_attribute(("sensitive", true));
        }
        out.push('\n');
        out.push_str(&fmt_block(&b.build()));
    }
    out
}

pub fn render_outputs(header: &str, outputs: &[OutputSpec]) -> String {
    let mut out = String::from(header);
    if outputs.is_empty() {
        out.push_str("\n# No outputs are declared for this configuration.\n");
        return out;
    }
    for o in outputs {
        let mut b = Block::builder("output")
            .add_label(o.name.as_str())
            .add_attribute(("description", o.description.as_str()))
            .add_attribute(("value", o.value.clone()));
        if o.sensitive {
            b = b.add_attribute(("sensitive", true));
        }
        let b = b.build();
        out.push('\n');
        out.push_str(&fmt_block(&b));
    }
    out
}

/// What `versions.tf` says beyond the provider list: the constraint for the project's
/// provider (its definition's default or the project's pin), the tool version the state
/// features need, and the `backend` / `encryption` blocks that share the `terraform {}`
/// block.
pub struct VersionsSpec<'a> {
    pub provider_version: &'a str,
    pub required_version: &'a str,
    pub extra_blocks: &'a [Block],
}

/// `versions.tf`. `helpers` are the side providers some emitted block actually draws a
/// resource from (`hashicorp/random` for generated secret values); an unused one is left
/// out so `init` never downloads a provider the configuration does not reference.
pub fn render_versions(
    header: &str,
    profile: &Profile,
    pdef: &ProviderDef,
    helpers: &[&HelperProviderDef],
    spec: &VersionsSpec,
) -> String {
    let entry = |source: String, version: &str| {
        let mut o = Object::new();
        o.insert(
            ObjectKey::Identifier(Identifier::unchecked("source")),
            Expression::String(source),
        );
        o.insert(
            ObjectKey::Identifier(Identifier::unchecked("version")),
            Expression::String(version.to_string()),
        );
        Expression::Object(o)
    };
    let mut rp = Block::builder("required_providers").add_attribute((
        pdef.provider.local_name(),
        entry(
            profile.provider_source(&pdef.provider.source_namespace, &pdef.provider.source_name),
            spec.provider_version,
        ),
    ));
    for h in helpers {
        let (ns, name) = h.source_parts();
        rp = rp.add_attribute((
            h.local_name(),
            entry(profile.provider_source(ns, name), &h.version),
        ));
    }
    let rp = rp.build();
    let mut tf = Block::builder("terraform")
        .add_attribute(("required_version", spec.required_version))
        .add_block(rp);
    for b in spec.extra_blocks {
        tf = tf.add_block(b.clone());
    }
    format!("{header}\n{}", fmt_block(&tf.build()))
}

/// `moved.tf`: where repeated blocks named by row index before they were named by key
/// have gone, so an existing state follows the rename.
pub fn render_moved(header: &str, blocks: &[Block]) -> String {
    let mut out = String::from(header);
    out.push_str(
        "\n# Resources made once per row or per linked resource are named after the row's key\n\
         # (a repository's name, a rule's name, the linked resource's name), not its position,\n\
         # so reordering a list no longer replaces them. These blocks move the old index-based\n\
         # addresses in an existing state to the new names on the next apply; they do nothing\n\
         # for a state that never had the old names. Once every state using this configuration\n\
         # has been applied, this file can be deleted (a later release stops writing it).\n",
    );
    for b in blocks {
        out.push('\n');
        out.push_str(&fmt_block(b));
    }
    out
}

/// `providers.tf`: the project's own provider configuration, followed by the aliased ones
/// some emitted resource sends itself to (`provider = aws.us_east_1`).
pub fn render_providers(header: &str, blocks: &[Block]) -> String {
    let mut out = String::from(header);
    for b in blocks {
        out.push('\n');
        out.push_str(&fmt_block(b));
    }
    out
}

pub fn render_manual_steps(
    p: &Project,
    cat: &Catalog,
    provider_display: &str,
    steps: &[ManualEntry],
    vars: &indexmap::IndexMap<String, VarSpec>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Manual steps — {provider_display}\n\n\
         Generated by TerraTofu GUI from project **{}**. The configuration in this directory \
         deploys, but the diagram contains intent that the {provider_display} mapping could not \
         fully express. Work through every item below.\n",
        p.name
    ));
    let mut n = 0;
    for s in steps {
        n += 1;
        out.push_str(&format!("\n## {n}. {}\n\n", s.title));
        if !s.body.is_empty() {
            out.push_str(s.body.trim());
            out.push('\n');
        }
        // For "create by hand" entries, list the configured values and the variables to fill.
        if let Some(id) = &s.entity {
            if s.title.starts_with("Create ") {
                if let Some(e) = p.entity(id) {
                    let def = cat.resource(e.resource_type);
                    out.push_str("\nConfigured in the diagram:\n\n");
                    out.push_str(&format!("- name: `{}`\n", e.name));
                    out.push_str(&format!("- abstract type: `{}`\n", e.resource_type));
                    if let Some(parent) = e.parent {
                        out.push_str(&format!("- inside: `{}`\n", p.hcl_name(parent)));
                    }
                    for (k, v) in e.config {
                        let label = def
                            .and_then(|d| d.fields.iter().find(|f| &f.name == k))
                            .map(|f| f.label().to_string())
                            .unwrap_or(k.clone());
                        out.push_str(&format!("- {label}: `{v}`\n"));
                    }
                    for (prov, cfg) in e.provider_config {
                        for (k, v) in cfg {
                            if !v.is_empty() {
                                out.push_str(&format!("- {k} ({prov}): `{v}`\n"));
                            }
                        }
                    }
                    let slug = p.hcl_name(id);
                    let needed: Vec<&VarSpec> = vars
                        .values()
                        .filter(|v| v.name.starts_with(&format!("{slug}_")) && v.default.is_none())
                        .collect();
                    if !needed.is_empty() {
                        out.push_str("\nAfter creating it, supply these input variables (e.g. in `terraform.tfvars`):\n\n");
                        for v in needed {
                            out.push_str(&format!("- `{}` — {}\n", v.name, v.description));
                        }
                    }
                }
            }
        }
    }
    out
}

/// What the README says about the state.
pub struct StateSummary {
    /// `backend "<type>"` and where the state object is, or `None` for local state.
    pub backend: Option<(String, String)>,
    /// The local state file when there is no remote backend.
    pub local_path: String,
    /// How the state is encrypted, if it is.
    pub encryption: Option<String>,
    /// The bootstrap roots, as `(directory, what it creates)`.
    pub bootstrap: Vec<(String, String)>,
}

impl StateSummary {
    pub fn of(
        p: &Project,
        enc: Option<&crate::state::EncryptionPlan>,
        roots: &[crate::state::BootstrapRoot],
    ) -> StateSummary {
        use crate::state::{self, KeySource};
        // With named environments there is one state per environment; list them all.
        let envs: Vec<Option<&str>> = if p.settings.environments.is_empty() {
            vec![None]
        } else {
            p.settings.environments.iter().map(|e| Some(e.as_str())).collect()
        };
        let each = |f: &dyn Fn(Option<&str>) -> String| {
            envs.iter()
                .map(|e| match e {
                    Some(name) => format!("{} ({name})", f(*e)),
                    None => f(None),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let backend = state::configured_backend(p)
            .filter(|b| b.backend_type != "local")
            .map(|b| {
                let at = match b.backend_type.as_str() {
                    "s3" => each(&|e| {
                        format!(
                            "s3://{}/{}",
                            b.args.get("bucket").cloned().unwrap_or_default(),
                            state::state_key(p, &b, e)
                        )
                    }),
                    "gcs" => each(&|e| {
                        format!(
                            "gs://{}/{}/default.tfstate",
                            b.args.get("bucket").cloned().unwrap_or_default(),
                            state::state_prefix(p, &b, e)
                        )
                    }),
                    _ => format!(
                        "storage account `{}`, container `{}`, blob {}",
                        b.args.get("storage_account_name").cloned().unwrap_or_default(),
                        b.args.get("container_name").cloned().unwrap_or_default(),
                        each(&|e| format!("`{}`", state::state_key(p, &b, e)))
                    ),
                };
                (b.backend_type.clone(), at)
            });
        let key_name = |id: &Option<String>| {
            id.as_deref()
                .and_then(|k| p.entity(k))
                .map(|e| e.name.to_string())
                .unwrap_or_default()
        };
        let encryption = enc.map(|e| match e.source {
            KeySource::AwsKms => format!(
                "AES-GCM with data keys from the AWS KMS key \"{}\" (`aws_kms` key provider)",
                key_name(&e.key)
            ),
            KeySource::GcpKms => format!(
                "AES-GCM with a key wrapped by the Cloud KMS key \"{}\" (`gcp_kms` key provider)",
                key_name(&e.key)
            ),
            KeySource::Passphrase => format!(
                "AES-GCM with a key derived from the `{}` variable (`pbkdf2` key provider)",
                state::PASSPHRASE_VAR
            ),
        });
        let bootstrap = roots
            .iter()
            .map(|r| {
                let what = match (r.store, &r.key) {
                    (true, Some(_)) => format!("the state store and the key \"{}\"", key_name(&r.key)),
                    (true, None) => "the state store".to_string(),
                    (false, _) => format!("the key \"{}\"", key_name(&r.key)),
                };
                (r.dir.clone(), what)
            })
            .collect();
        let local_path = if p.settings.environments.is_empty() {
            state::configured_backend(p)
                .and_then(|b| b.args.get("path").cloned())
                .unwrap_or("terraform.tfstate".into())
        } else {
            each(&|e| format!("`{}`", state::environment_arg_value(p, e.unwrap_or_default()).1))
        };
        StateSummary {
            backend,
            local_path,
            encryption,
            bootstrap,
        }
    }
}

pub fn render_readme(
    p: &Project,
    profile: &Profile,
    pdef: &ProviderDef,
    has_manual: bool,
    has_manifests: bool,
    state: &StateSummary,
) -> String {
    let bin = profile.binary();
    let usage = match p.settings.environments.first() {
        None => format!("{bin} init\n{bin} validate\n{bin} plan\n{bin} apply"),
        Some(env) => format!(
            "{bin} init -reconfigure -backend-config=environments/{env}.backend.hcl\n{bin} validate\n\
             {bin} plan -var-file=environments/{env}.tfvars -out={env}.tfplan\n{bin} apply {env}.tfplan"
        ),
    };
    let mut s = format!(
        "# {} — {} ({})\n\n\
         Generated by TerraTofu GUI. Provider: **{}** (`{}` {}).\n\n\
         ## Usage\n\n```\n{usage}\n```\n\n\
         Input variables are declared in `variables.tf`; put values in a `terraform.tfvars` file \
         or pass `-var`. Provider docs: {}\n",
        p.name,
        pdef.provider.display_name,
        profile.display_name(),
        pdef.provider.display_name,
        profile.provider_source(&pdef.provider.source_namespace, &pdef.provider.source_name),
        crate::versions::constraint(p, pdef),
        profile.registry_url(&pdef.provider.source_namespace, &pdef.provider.source_name),
    );
    if has_manual {
        s.push_str("\n> **Read `MANUAL_STEPS.md`** — parts of the diagram must be completed by hand.\n");
    }
    s.push_str(&format!(
        "\n## Provider lock file\n\n\
         This directory has no `.terraform.lock.hcl`. The one `{bin} init` writes holds the \
         provider checksums of the platform it ran on only, and `init` on any other platform \
         (a Linux CI runner, a colleague's Mac) then refuses the providers it downloads. \
         Before committing, lock for every platform that will run this configuration and \
         commit the result:\n\n```\n{}\n```\n\n\
         (`ttg export --lock` runs the same command after exporting.) Re-exporting never \
         touches the lock file.\n",
        crate::validate::lock_command(profile.tool)
    ));
    if has_manifests {
        s.push_str(&format!(
            "\n## Kubernetes manifests\n\n\
             `k8s/` holds the Kubernetes objects for the workloads in the diagram. After \
             `{bin} apply`, run `k8s/render.sh` (or `k8s/render.ps1`) to fill in the values \
             only Terraform knows — they are the `k8s_*` outputs in `outputs.tf` — then \
             `kubectl apply -f k8s/rendered/`. See `k8s/README.md`.\n"
        ));
    }
    s.push_str("\n## State\n\n");
    match &state.backend {
        Some((kind, at)) => s.push_str(&format!(
            "The state is kept remotely (`backend \"{kind}\"` in `versions.tf`): {at}.{}\n",
            if kind == "s3" {
                " Locking uses an S3 lock file (`use_lockfile`), so there is no DynamoDB table."
            } else {
                ""
            }
        )),
        None => s.push_str(&format!(
            "The state is a local file ({}). It records every attribute of every resource, \
             generated secrets included: configure a remote backend and, with OpenTofu, state \
             encryption in the app's Settings before sharing it.\n",
            if p.settings.environments.is_empty() {
                format!("`{}`", state.local_path)
            } else {
                format!("one per environment: {}", state.local_path)
            }
        )),
    }
    match &state.encryption {
        Some(how) => s.push_str(&format!(
            "\nThe state and saved plans are encrypted by {} ({how}) and cannot be read or written \
             unencrypted (`enforced = true`). To encrypt a state that already exists unencrypted, \
             add `method \"unencrypted\" \"migrate\" {{}}` and `fallback {{ method = \
             method.unencrypted.migrate }}` to the `state` block for one apply, then remove them.\n",
            profile.display_name()
        )),
        None if p.settings.state_encryption => s.push_str(&format!(
            "\nState encryption is on in the project, but {} has no `encryption` block: the \
             state is not encrypted. Export for OpenTofu to encrypt it.\n",
            profile.display_name()
        )),
        None => {}
    }
    if !state.bootstrap.is_empty() {
        s.push_str(
            "\n### Bootstrap\n\nWhat the state needs must exist before this configuration can use \
             it, so it is created by a separate configuration, applied first (see \
             `MANUAL_STEPS.md`):\n\n",
        );
        for (dir, what) in &state.bootstrap {
            s.push_str(&format!("- `{dir}/` creates {what}.\n"));
        }
    }
    s
}

fn raw(s: &str) -> Expression {
    crate::emit::raw_expr(s)
}

/// Pad `key =` lines so their `=` signs line up the way `terraform fmt` / `tofu fmt` do
/// it (hclwrite's `formatCells`): a line takes part when it has an `=` after its first
/// token whose right-hand side closes every bracket it opens, and consecutive such lines
/// form one group, whatever their nesting. Anything else — a nested block's header or
/// closing brace, a value that runs over several lines, a blank line, a comment — ends
/// the group. Each group is aligned to its longest left-hand side.
pub fn align_attributes(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let eq: Vec<Option<usize>> = lines.iter().map(|l| assign_at(l)).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if eq[i].is_none() {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        let mut width = 0;
        while j < lines.len() {
            let Some(at) = eq[j] else { break };
            width = width.max(lines[j][..at].trim_end().chars().count());
            j += 1;
        }
        for k in i..j {
            let at = eq[k].unwrap();
            let lead = lines[k][..at].trim_end();
            let pad = width - lead.chars().count();
            out.push(format!("{lead}{} {}", " ".repeat(pad), &lines[k][at..]));
        }
        i = j;
    }
    out.join("\n")
}

/// Byte offset of the `=` that makes `line` an attribute line in hclwrite's sense, or
/// `None`. The `=` must come after the first token and outside strings, must not be part
/// of `==`, `!=`, `<=`, `>=` or `=>`, and the rest of the line must close every bracket
/// it opens (a value that continues on the next lines is not aligned).
fn assign_at(line: &str) -> Option<usize> {
    #[derive(Clone, Copy, PartialEq)]
    enum Frame {
        Code,
        Str,
        /// `${ … }` / `%{ … }` inside a string, with its own brace depth.
        Interp(u32),
    }
    let bytes = line.as_bytes();
    let first = line.len() - line.trim_start().len();
    if first == line.len() {
        return None;
    }
    let mut stack = vec![Frame::Code];
    let mut eq: Option<usize> = None;
    let mut net: i32 = 0;
    let mut i = first;
    while i < bytes.len() {
        let c = bytes[i];
        let top = *stack.last().unwrap();
        match top {
            Frame::Str => match c {
                b'\\' => i += 1,
                b'"' => {
                    stack.pop();
                }
                b'$' | b'%' if bytes.get(i + 1) == Some(&c) => i += 1,
                b'$' | b'%' if bytes.get(i + 1) == Some(&b'{') => {
                    stack.push(Frame::Interp(0));
                    i += 1;
                }
                _ => {}
            },
            Frame::Interp(depth) => match c {
                b'"' => stack.push(Frame::Str),
                b'{' => *stack.last_mut().unwrap() = Frame::Interp(depth + 1),
                b'}' if depth == 0 => {
                    stack.pop();
                }
                b'}' => *stack.last_mut().unwrap() = Frame::Interp(depth - 1),
                _ => {}
            },
            Frame::Code => {
                let top_level = stack.len() == 1;
                match c {
                    b'"' => stack.push(Frame::Str),
                    b'#' => break,
                    b'/' if bytes.get(i + 1) == Some(&b'/') => break,
                    b'{' | b'[' | b'(' if top_level && eq.is_some() => net += 1,
                    b'}' | b']' | b')' if top_level && eq.is_some() => net -= 1,
                    b'=' if top_level && eq.is_none() && i > first => {
                        let prev = bytes[i - 1];
                        let next = bytes.get(i + 1).copied();
                        let operator = matches!(prev, b'=' | b'!' | b'<' | b'>')
                            || matches!(next, Some(b'=') | Some(b'>'));
                        if !operator {
                            eq = Some(i);
                        }
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    eq.filter(|_| net == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align() {
        let s = "resource \"a\" \"b\" {\n  id = 1\n  cidr_block = \"x\"\n\n  nested {\n    a = 1\n  }\n}\n";
        let a = align_attributes(s);
        assert!(a.contains("  id         = 1\n  cidr_block = \"x\""));
        assert!(a.contains("    a = 1"));
    }

    #[test]
    fn object_for_expressions_are_padded() {
        assert_eq!(
            space_object_for(
                "  for_each = {for o in x : o.k => o}\n  s = \"{for}\"\n  l = [for v in y : v]\n"
            ),
            "  for_each = { for o in x : o.k => o }\n  s = \"{for}\"\n  l = [for v in y : v]\n"
        );
    }

    /// The groups `terraform fmt` makes: a multi-line value's first line takes no part,
    /// its inner lines align with each other across nesting, and a closing brace ends a
    /// group.
    #[test]
    fn align_like_terraform_fmt() {
        let s = "resource \"a\" \"b\" {\n  name = \"x\"\n  tags = {\n    Name = \"x\"\n    Environment = \"y\"\n  }\n  ab = 1\n  policy = jsonencode({\n    a = 1\n  })\n  c = \"${a == b}=\"\n  longer_name = x == y ? 1 : 2\n}\n";
        let a = align_attributes(s);
        assert_eq!(
            a,
            "resource \"a\" \"b\" {\n  name = \"x\"\n  tags = {\n    Name        = \"x\"\n    Environment = \"y\"\n  }\n  ab = 1\n  policy = jsonencode({\n    a = 1\n  })\n  c           = \"${a == b}=\"\n  longer_name = x == y ? 1 : 2\n}\n"
        );
        assert_eq!(assign_at("  a = { b = 1 }"), Some(4));
        assert_eq!(assign_at("  a = {"), None);
        assert_eq!(assign_at("  for_each = { for o in x : o.k => o }"), Some(11));
        assert_eq!(assign_at("  \"aws:x\" = \"y\""), Some(10));
        assert_eq!(assign_at("    x == y"), None);
        assert_eq!(assign_at("# a = b"), None);
    }
}
