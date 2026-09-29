//! State and provider versions: the backend block in `versions.tf`, the bootstrap root,
//! OpenTofu state encryption (and its absence on Terraform), the warning for generated
//! secrets in unprotected state, and the provider version pins.

use std::collections::BTreeMap;
use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::state::{self, check_backend};
use ttg_codegen::{diagnostics, generate, Code, Diagnostic, Severity};
use ttg_core::{BackendConfig, Project, Tool};

fn example(name: &str) -> Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

fn backend(kind: &str, args: &[(&str, &str)]) -> BackendConfig {
    BackendConfig {
        backend_type: kind.into(),
        args: args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn s3() -> BackendConfig {
    backend(
        "s3",
        &[("bucket", "hardened-tfstate-x7q2"), ("region", "eu-west-2")],
    )
}

fn azurerm() -> BackendConfig {
    backend(
        "azurerm",
        &[
            ("resource_group_name", "tfstate_rg"),
            ("storage_account_name", "hardenedtfstatex7q2"),
            ("container_name", "tfstate"),
        ],
    )
}

fn gcs() -> BackendConfig {
    backend("gcs", &[("bucket", "hardened-tfstate-x7q2")])
}

/// `hardened` has an Encryption Key ("data key", `key-main`) that most of its resources
/// are encrypted with, and a Secret with a generated value ("db password", `sec-db`).
fn hardened_with(b: Option<BackendConfig>, encrypt: bool) -> Project {
    let mut p = example("hardened.ttg.json");
    p.settings.backend = b;
    p.settings.state_encryption = encrypt;
    p.settings.state_encryption_key = encrypt.then(|| "key-main".to_string());
    p
}

fn state_diags(d: &[Diagnostic]) -> Vec<&Diagnostic> {
    d.iter().filter(|d| d.code == Code::State).collect()
}

#[test]
fn s3_backend_goes_into_the_terraform_block_with_a_bootstrap_root() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(s3()), false);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    // One terraform block: versions, providers and the backend together.
    assert_eq!(v.matches("terraform {").count(), 1, "{v}");
    for want in [
        "backend \"s3\" {",
        "bucket       = \"hardened-tfstate-x7q2\"",
        "key          = \"hardened/terraform.tfstate\"",
        "region       = \"eu-west-2\"",
        "encrypt      = true",
        "use_lockfile = true",
        // S3 lock files need OpenTofu 1.10.
        "required_version = \">= 1.10.0\"",
    ] {
        assert!(v.contains(want), "missing {want}:\n{v}");
    }
    assert!(!v.contains("dynamodb"), "{v}");
    assert!(!g.files.contains_key("backend.tf"));

    // The bootstrap root: the bucket, with the posture of any curated bucket.
    let bs = &g.files["bootstrap/storage.tf"];
    for want in [
        "resource \"aws_s3_bucket\" \"hardened_state\"",
        "bucket = \"hardened-tfstate-x7q2\"",
        "resource \"aws_s3_bucket_versioning\"",
        "status = \"Enabled\"",
        "resource \"aws_s3_bucket_public_access_block\"",
        "block_public_policy     = true",
        "DenyInsecureTransport",
    ] {
        assert!(bs.contains(want), "missing {want}:\n{bs}");
    }
    // It keeps its own state locally, and is in the backend's region.
    assert!(!g.files["bootstrap/versions.tf"].contains("backend"));
    assert!(g.files["bootstrap/variables.tf"].contains("default     = \"eu-west-2\""));
    // No key without encryption.
    assert!(!g.files.contains_key("bootstrap/security.tf"));
    assert!(g.files["security.tf"].contains("resource \"aws_kms_key\" \"data_key\""));

    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(steps.contains("## 1. Apply the bootstrap root first"), "{steps}");
    assert!(steps.contains("tofu -chdir=bootstrap apply"), "{steps}");
    let readme = &g.files["README.md"];
    assert!(
        readme.contains("s3://hardened-tfstate-x7q2/hardened/terraform.tfstate"),
        "{readme}"
    );

    // Terraform needs 1.11 for S3 lock files.
    let tf = generate(&p, &cat, "aws", Tool::Terraform).unwrap();
    assert!(tf.files["versions.tf"].contains("required_version = \">= 1.11.0\""));
    assert!(tf.files.contains_key("bootstrap/storage.tf"));
}

#[test]
fn s3_backend_options_and_the_key_prefix() {
    let cat = Catalog::builtin();
    let mut b = s3();
    b.args.insert("key_prefix".into(), "platform/hardened".into());
    b.args.insert(
        "kms_key_id".into(),
        "arn:aws:kms:eu-west-2:111122223333:key/abc".into(),
    );
    let p = hardened_with(Some(b.clone()), false);
    let v = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["versions.tf"].clone();
    assert!(
        v.contains("key          = \"platform/hardened/terraform.tfstate\""),
        "{v}"
    );
    assert!(
        v.contains("kms_key_id   = \"arn:aws:kms:eu-west-2:111122223333:key/abc\""),
        "{v}"
    );
    // Room for a named environment between the prefix and the file.
    assert_eq!(
        state::state_key(&p, &b, Some("prod")),
        "platform/hardened/prod/terraform.tfstate"
    );
    assert_eq!(
        state::state_key(&p, &b, None),
        "platform/hardened/terraform.tfstate"
    );
    // An explicit key gets the environment in front of its file name.
    let explicit = backend(
        "s3",
        &[("bucket", "b"), ("region", "r"), ("key", "net/terraform.tfstate")],
    );
    assert_eq!(state::state_key(&p, &explicit, None), "net/terraform.tfstate");
    assert_eq!(
        state::state_key(&p, &explicit, Some("prod")),
        "net/prod/terraform.tfstate"
    );
    // The gcs backend's own `prefix` means key_prefix.
    let g = backend("gcs", &[("bucket", "b"), ("prefix", "team")]);
    assert_eq!(state::state_prefix(&p, &g, Some("dev")), "team/dev");
}

#[test]
fn azurerm_backend_and_its_bootstrap_root() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(azurerm()), false);
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    for want in [
        "backend \"azurerm\" {",
        "resource_group_name  = \"tfstate_rg\"",
        "storage_account_name = \"hardenedtfstatex7q2\"",
        "container_name       = \"tfstate\"",
        "key                  = \"hardened/terraform.tfstate\"",
    ] {
        assert!(v.contains(want), "missing {want}:\n{v}");
    }
    // The resource group keeps the exact name the backend expects (the mapping alone
    // would kebab-case it to `tfstate-rg`).
    let rg = &g.files["bootstrap/main.tf"];
    assert!(rg.contains("name     = \"tfstate_rg\""), "{rg}");
    let st = &g.files["bootstrap/storage.tf"];
    for want in [
        "name                            = \"hardenedtfstatex7q2\"",
        "resource_group_name             = azurerm_resource_group.tfstate_rg.name",
        "allow_nested_items_to_be_public = false",
        "https_traffic_only_enabled      = true",
        "versioning_enabled = true",
        "resource \"azurerm_storage_container\"",
        "name                  = \"tfstate\"",
    ] {
        assert!(st.contains(want), "missing {want}:\n{st}");
    }
}

#[test]
fn gcs_backend_and_its_bootstrap_root() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(gcs()), false);
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    assert!(v.contains("backend \"gcs\" {"), "{v}");
    assert!(v.contains("bucket = \"hardened-tfstate-x7q2\""), "{v}");
    assert!(v.contains("prefix = \"hardened\""), "{v}");
    // No S3 lock file, no encryption: the tool floor stays where it was.
    assert!(v.contains("required_version = \">= 1.6.0\""), "{v}");
    let st = &g.files["bootstrap/storage.tf"];
    for want in [
        "resource \"google_storage_bucket\" \"hardened_state\"",
        "name                        = \"hardened-tfstate-x7q2\"",
        "uniform_bucket_level_access = true",
        "public_access_prevention    = \"enforced\"",
        "versioning {",
    ] {
        assert!(st.contains(want), "missing {want}:\n{st}");
    }
}

#[test]
fn opentofu_encrypts_the_state_with_the_chosen_kms_key_from_the_bootstrap_root() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(s3()), true);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    for want in [
        "encryption {",
        "key_provider \"aws_kms\" \"state\" {",
        "kms_key_id = var.data_key_arn",
        "region     = \"eu-west-2\"",
        "key_spec   = \"AES_256\"",
        "method \"aes_gcm\" \"state\" {",
        "keys = key_provider.aws_kms.state",
        "state {",
        "plan {",
        "method   = method.aes_gcm.state",
        "enforced = true",
    ] {
        assert!(v.contains(want), "missing {want}:\n{v}");
    }
    // The key is not in the root whose state it encrypts: the bootstrap root creates
    // it, with its own mapping (key policy included), and outputs its ARN.
    assert!(!g.files.contains_key("security.tf") || !g.files["security.tf"].contains("aws_kms_key"));
    let bs = &g.files["bootstrap/security.tf"];
    assert!(bs.contains("resource \"aws_kms_key\" \"data_key\""), "{bs}");
    assert!(bs.contains("CloudWatchLogs"), "{bs}");
    assert!(g.files["bootstrap/outputs.tf"].contains("output \"data_key_arn\""));
    // Everything else encrypted with it takes the same variable.
    assert!(g.files["secrets.tf"].contains("kms_key_id              = var.data_key_arn"));
    let vars = &g.files["variables.tf"];
    assert!(vars.contains("variable \"data_key_arn\""), "{vars}");
    assert!(
        vars.contains("output `data_key_arn` of the bootstrap root"),
        "{vars}"
    );
    assert!(!vars.contains("state_passphrase"), "{vars}");
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(
        steps.contains("`data_key_arn` = `tofu -chdir=bootstrap output -raw data_key_arn`"),
        "{steps}"
    );
    assert!(!steps.contains("Create Encryption Key"), "{steps}");
    // With a generated secret, a remote backend and encryption, nothing to warn about.
    assert!(state_diags(&g.diagnostics).is_empty(), "{:?}", g.diagnostics);
}

#[test]
fn gcp_uses_the_cloud_kms_key_provider() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(gcs()), true);
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    assert!(v.contains("key_provider \"gcp_kms\" \"state\" {"), "{v}");
    assert!(v.contains("kms_encryption_key = var.data_key_id"), "{v}");
    assert!(v.contains("key_length         = 32"), "{v}");
    assert!(g.files["bootstrap/security.tf"].contains("resource \"google_kms_crypto_key\" \"data_key\""));
    // The key's own manual steps stay with it, and the main list points there.
    assert!(g.files.contains_key("bootstrap/MANUAL_STEPS.md"));
    assert!(g.files["MANUAL_STEPS.md"].contains("read `bootstrap/MANUAL_STEPS.md` first"));
}

#[test]
fn azure_encrypts_with_a_passphrase_and_says_why() {
    let cat = Catalog::builtin();
    let p = hardened_with(Some(azurerm()), true);
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let v = &g.files["versions.tf"];
    assert!(v.contains("key_provider \"pbkdf2\" \"state\" {"), "{v}");
    assert!(v.contains("passphrase = var.state_passphrase"), "{v}");
    assert!(g.files["variables.tf"].contains("variable \"state_passphrase\""));
    // The key stays in the main root (it is a Key Vault key there) and no bootstrap key.
    assert!(g.files["security.tf"].contains("azurerm_key_vault_key"));
    assert!(!g.files.contains_key("bootstrap/security.tf"));
    let info = state_diags(&g.diagnostics);
    assert!(
        info.iter()
            .any(|d| d.severity == Severity::Info && d.message.contains("azure_vault")),
        "{info:?}"
    );
}

#[test]
fn terraform_gets_no_encryption_block_and_a_warning() {
    let cat = Catalog::builtin();
    let mut p = hardened_with(Some(s3()), true);
    p.settings.tool = Tool::Terraform;
    let g = generate(&p, &cat, "aws", Tool::Terraform).unwrap();
    assert!(
        !g.files["versions.tf"].contains("encryption"),
        "{}",
        g.files["versions.tf"]
    );
    assert!(!g.files["variables.tf"].contains("state_passphrase"));
    // Nothing is encrypted with it, so the key is an ordinary part of the configuration.
    assert!(g.files["security.tf"].contains("resource \"aws_kms_key\" \"data_key\""));
    assert!(!g.files.contains_key("bootstrap/security.tf"));
    let d = diagnostics::run(&p, &cat, "aws");
    let w: Vec<_> = state_diags(&d)
        .into_iter()
        .filter(|d| d.severity == Severity::Warning)
        .collect();
    assert!(
        w.iter()
            .any(|d| d.message.contains("state encryption needs OpenTofu")),
        "{w:?}"
    );
    // ... and the generated secret sits in plain text in the remote state.
    assert!(
        w.iter()
            .any(|d| d.entity.as_deref() == Some("sec-db")
                && d.message.contains("plain text in the remote state")),
        "{w:?}"
    );

    // Exporting the other flavour answers for that flavour, not the project's tool.
    p.settings.tool = Tool::OpenTofu;
    let g = generate(&p, &cat, "aws", Tool::Terraform).unwrap();
    assert!(state_diags(&g.diagnostics)
        .iter()
        .any(|d| d.message.contains("state encryption needs OpenTofu")));
}

#[test]
fn generated_secret_warns_while_the_state_is_local_or_unencrypted() {
    let cat = Catalog::builtin();
    let warning = |p: &Project| -> Option<String> {
        diagnostics::run(p, &cat, "aws")
            .into_iter()
            .find(|d| d.code == Code::State && d.entity.as_deref() == Some("sec-db"))
            .map(|d| {
                assert_eq!(d.severity, Severity::Warning);
                d.message
            })
    };
    let local = warning(&hardened_with(None, false)).expect("local, unencrypted");
    assert!(
        local.ends_with(
            "the generated value is stored in plain text in local state; configure a remote backend and state encryption (Settings)"
        ),
        "{local}"
    );
    let remote = warning(&hardened_with(Some(s3()), false)).expect("remote, unencrypted");
    assert!(
        remote.contains("plain text in the remote state (s3 backend)"),
        "{remote}"
    );
    let local_enc = warning(&hardened_with(None, true)).expect("local, encrypted");
    assert!(local_enc.contains("configure a remote backend"), "{local_enc}");
    assert!(warning(&hardened_with(Some(s3()), true)).is_none());

    // Without a generated value there is nothing in the state to worry about.
    let mut p = hardened_with(None, false);
    p.nodes
        .get_mut("sec-db")
        .unwrap()
        .config
        .insert("generate_value".into(), ttg_core::Value::Bool(false));
    assert!(warning(&p).is_none());
}

#[test]
fn state_and_store_on_different_clouds_get_two_bootstrap_roots() {
    let cat = Catalog::builtin();
    // The Google Cloud export, its state in S3, encrypted with a Cloud KMS key.
    let p = hardened_with(Some(s3()), true);
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    assert!(g.files["bootstrap/storage.tf"].contains("resource \"aws_s3_bucket\""));
    assert!(g.files["bootstrap/gcp/security.tf"].contains("google_kms_crypto_key"));
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(steps.contains("tofu -chdir=bootstrap/gcp apply"), "{steps}");
    assert!(
        steps.contains("tofu -chdir=bootstrap/gcp output -raw data_key_id"),
        "{steps}"
    );
    let d = state_diags(&g.diagnostics);
    assert!(
        d.iter()
            .any(|d| d.severity == Severity::Info && d.message.contains("keeps its state in Amazon S3")),
        "{d:?}"
    );
}

#[test]
fn backend_settings_are_checked() {
    assert!(check_backend(&s3()).is_ok());
    assert!(check_backend(&azurerm()).is_ok());
    assert!(check_backend(&gcs()).is_ok());
    assert!(check_backend(&backend("local", &[])).is_ok());
    let err = |b: BackendConfig| check_backend(&b).unwrap_err();
    let e = err(backend("consul", &[]));
    assert!(
        e.contains("unknown backend type \"consul\"; use one of: local, s3, azurerm, gcs"),
        "{e}"
    );
    let e = err(backend("s3", &[("bucket", "b")]));
    assert!(
        e.contains("the s3 backend needs bucket, region; missing: region"),
        "{e}"
    );
    let e = err(backend(
        "s3",
        &[("bucket", "b"), ("region", "r"), ("dynamodb_table", "t")],
    ));
    assert!(e.contains("\"dynamodb_table\" is not one of them"), "{e}");
    assert!(e.contains("bucket, region, key_prefix, key, kms_key_id"), "{e}");
    // A blank value is "not set", so a required key left blank is missing.
    let e = err(backend("gcs", &[("bucket", "  ")]));
    assert!(e.contains("missing: bucket"), "{e}");
    // An explicit key (what the settings panel used to offer) still works, but not
    // together with key_prefix.
    assert!(check_backend(&backend(
        "s3",
        &[("bucket", "b"), ("region", "r"), ("key", "x/terraform.tfstate")]
    ))
    .is_ok());
    let e = err(backend(
        "s3",
        &[
            ("bucket", "b"),
            ("region", "r"),
            ("key", "x/terraform.tfstate"),
            ("key_prefix", "x"),
        ],
    ));
    assert!(e.contains("set key_prefix or key, not both"), "{e}");
    let e = err(backend("gcs", &[("bucket", "b"), ("key_prefix", "/abs/")]));
    assert!(e.contains("relative path"), "{e}");

    // An invalid backend in the file blocks the export with the same message.
    let cat = Catalog::builtin();
    let p = hardened_with(Some(backend("s3", &[("bucket", "b")])), false);
    let e = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_err().to_string();
    assert!(
        e.contains("State backend: the s3 backend needs bucket, region"),
        "{e}"
    );
}

#[test]
fn backend_json_shapes() {
    use serde_json::json;
    let flat = state::backend_from_json(&json!({"type": "s3", "bucket": "b", "region": "r"}))
        .unwrap()
        .unwrap();
    // The shape project_get returns is accepted too, so a read can be written back.
    let nested = state::backend_from_json(&serde_json::to_value(&flat).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(flat, nested);
    assert_eq!(flat.args["bucket"], "b");
    assert!(state::backend_from_json(&json!(null)).unwrap().is_none());
    assert!(state::backend_from_json(&json!({"type": "none"}))
        .unwrap()
        .is_none());
    assert!(state::backend_from_json(&json!({"bucket": "b"})).is_err());
    assert!(state::backend_from_json(&json!({"type": "s3", "bucket": true, "region": "r"})).is_err());
}

#[test]
fn what_the_bootstrap_root_would_refuse_blocks_the_export() {
    let cat = Catalog::builtin();
    let mut b = azurerm();
    b.args.insert("storage_account_name".into(), "Not_Valid".into());
    let p = hardened_with(Some(b), false);
    let d = diagnostics::run(&p, &cat, "azure");
    let e = d
        .iter()
        .find(|d| d.code == Code::State && d.severity == Severity::Error)
        .unwrap_or_else(|| panic!("{d:?}"));
    assert!(
        e.message.starts_with("State bootstrap (bootstrap/): "),
        "{}",
        e.message
    );
    assert!(generate(&p, &cat, "azure", Tool::OpenTofu).is_err());
}

#[test]
fn the_state_key_setting_must_name_an_encryption_key() {
    let cat = Catalog::builtin();
    let mut p = hardened_with(Some(s3()), true);
    p.settings.state_encryption_key = Some("sec-db".into());
    let d = diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter().any(|d| d.code == Code::State
            && d.severity == Severity::Error
            && d.message.contains("not an Encryption Key")),
        "{d:?}"
    );
    p.settings.state_encryption_key = Some("gone".into());
    let d = diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("does not exist")),
        "{d:?}"
    );
    // Encryption on without a key: a passphrase, and a pointer to the KMS option.
    p.settings.state_encryption_key = None;
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(g.files["versions.tf"].contains("key_provider \"pbkdf2\" \"state\""));
    assert!(state_diags(&g.diagnostics)
        .iter()
        .any(|d| d.severity == Severity::Info && d.message.contains("to use AWS KMS instead")));
}

// ------------------------------------------------------------------ provider versions

#[test]
fn a_provider_version_pin_goes_into_required_providers() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    let default = cat.provider("aws").unwrap().provider.version_constraint.clone();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(g.files["versions.tf"].contains(&format!("version = \"{default}\"")));
    p.settings
        .provider_versions
        .insert("aws".into(), ">= 6.0, < 7.0".into());
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(g.files["versions.tf"].contains("version = \">= 6.0, < 7.0\""));
    assert!(g.files["README.md"].contains(">= 6.0, < 7.0"));
    // A pin for another provider leaves this one alone.
    assert!(
        !generate(&p, &cat, "azure", Tool::OpenTofu).unwrap().files["versions.tf"].contains(">= 6.0, < 7.0")
    );
}

fn bundled_major(provider: &str) -> u64 {
    ttg_schema::index()
        .provider(provider)
        .unwrap()
        .version
        .split('.')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_pin_the_bundled_schema_does_not_describe_is_reported() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    let major = bundled_major("aws");
    p.settings
        .provider_versions
        .insert("aws".into(), format!("~> {}.0", major - 1));
    let d = diagnostics::run(&p, &cat, "aws");
    let v: Vec<_> = d.iter().filter(|d| d.code == Code::Version).collect();
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].severity, Severity::Info);
    assert!(
        v[0].message
            .starts_with(&format!("argument checks use aws {major}.x (the bundled schema")),
        "{}",
        v[0].message
    );
    assert!(v[0]
        .message
        .contains(&format!("your project pins ~> {}.0", major - 1)));
    // A malformed pin is an error.
    p.settings.provider_versions.insert("aws".into(), "latest".into());
    assert!(diagnostics::run(&p, &cat, "aws")
        .iter()
        .any(|d| d.code == Code::Version && d.severity == Severity::Error));
}

/// The acceptance test for R3.4: pin AWS to `~> 6.0` on every example, and the curated
/// mappings are checked against the AWS 6 schema — no mapping breaks. (The validate
/// suite, `validate_examples.rs`, runs `tofu validate` on the same exports; the default
/// constraint is `~> 6.0`, so that is exactly what it installs.) A mapping that did
/// break would be named: see the next test.
#[test]
fn switching_aws_to_6_names_no_broken_mapping() {
    let cat = Catalog::builtin();
    assert_eq!(bundled_major("aws"), 6, "the bundled AWS schema is 6.x");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let path = e.path();
        if !path.to_string_lossy().ends_with(".ttg.json") {
            continue;
        }
        let mut p = ttg_core::project::load(&path).unwrap();
        p.settings.provider_versions.insert("aws".into(), "~> 6.0".into());
        let mut cat = cat.clone();
        cat.ensure_native_types(&p);
        let d = diagnostics::run(&p, &cat, "aws");
        let v: Vec<_> = d.iter().filter(|d| d.code == Code::Version).collect();
        assert!(v.is_empty(), "{}: {v:?}", path.display());
        n += 1;
    }
    assert!(n > 5);
}

#[test]
fn a_mapping_that_breaks_on_the_pinned_version_is_named() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let thing = r#"
schema_version = 2
[resource]
type = "thing"
category = "storage"
display_name = "Thing"
[providers.aws]
status = "full"
[[providers.aws.blocks]]
key = "main"
resource = "aws_s3_bucket"
[providers.aws.blocks.args]
bucket = { value = "b" }
acceleration_status_v0 = { value = "Enabled" }
"#;
    let cat = Catalog::from_sources(
        [("thing.toml", thing)].into_iter(),
        [("aws.toml", aws)].into_iter(),
    )
    .unwrap();
    let mut p = Project::new("broken");
    p.nodes.insert(
        "thing-1".into(),
        serde_json::from_value(serde_json::json!({
            "id": "thing-1", "name": "t", "resource_type": "thing"
        }))
        .unwrap(),
    );
    p.settings.provider_versions.insert("aws".into(), "~> 6.0".into());
    let d = diagnostics::run(&p, &cat, "aws");
    let v = d
        .iter()
        .find(|d| d.code == Code::Version)
        .unwrap_or_else(|| panic!("{d:?}"));
    assert_eq!(v.severity, Severity::Warning);
    assert_eq!(v.entity.as_deref(), Some("thing-1"));
    assert!(
        v.message
            .contains("the aws mapping of thing (definitions/resources/thing.toml)"),
        "{}",
        v.message
    );
    assert!(
        v.message
            .contains("thing/aws/main (aws_s3_bucket): argument 'acceleration_status_v0' does not exist"),
        "{}",
        v.message
    );
}
