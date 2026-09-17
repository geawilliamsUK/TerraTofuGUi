//! End-to-end: example project -> both providers -> both tools.

use std::path::Path;
use ttg_catalog::Catalog;
use ttg_codegen::{generate, Severity};
use ttg_core::Tool;

fn example(name: &str) -> ttg_core::Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    ttg_core::project::load(&p).expect("example loads")
}

#[test]
fn three_tier_aws_opentofu() {
    let cat = Catalog::builtin();
    let p = example("three-tier.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let net = &g.files["network.tf"];
    assert!(net.contains("resource \"aws_vpc\" \"main\""));
    assert!(net.contains("= aws_vpc.main.id"));
    let compute = &g.files["compute.tf"];
    assert!(compute.contains("iam_instance_profile = aws_iam_instance_profile.app_role_profile.name"));
    assert!(compute.contains("= aws_subnet.web_a.id"));
    assert!(compute.contains("depends_on = [\n    aws_s3_bucket.assets\n  ]"));
    let iam = &g.files["iam.tf"];
    assert!(iam.contains("name = \"app-role\""));
    assert!(iam.contains("jsonencode({ Version = \"2012-10-17\""));
    assert!(iam.contains("policy_arn = \"arn:aws:iam::aws:policy/ReadOnlyAccess\""));
    assert!(g.files["storage.tf"].contains("resource \"aws_s3_bucket_versioning\" \"assets_versioning\""));
    assert!(g.files["versions.tf"].contains("registry.opentofu.org/hashicorp/aws"));
    assert!(g.files["versions.tf"].contains(">= 1.6.0"));
    assert!(g.files.contains_key("MANUAL_STEPS.md"));
    assert_eq!(g.manual_steps.len(), 1, "unconsumed edge -> one manual step");
    assert!(
        !g.files.contains_key("main.tf"),
        "resource group is logical on AWS"
    );
    assert!(g.diagnostics.iter().all(|d| d.severity != Severity::Error));
}

#[test]
fn three_tier_azure_terraform() {
    let cat = Catalog::builtin();
    let p = example("three-tier.ttg.json");
    let g = generate(&p, &cat, "azure", Tool::Terraform).unwrap();
    assert!(g.files["main.tf"].contains("resource \"azurerm_resource_group\" \"app\""));
    let compute = &g.files["compute.tf"];
    assert!(compute.contains("resource \"azurerm_network_interface\" \"app_server_nic\""));
    assert!(
        compute.contains("network_interface_ids = [\n    azurerm_network_interface.app_server_nic.id\n  ]")
    );
    assert!(compute.contains("identity {"));
    assert!(compute.contains("azurerm_user_assigned_identity.app_role.id"));
    assert!(compute.contains("public_key = var.ssh_public_key"));
    assert!(g.files["variables.tf"].contains("variable \"ssh_public_key\""));
    assert!(g.files["iam.tf"].contains("role_definition_name = \"Reader\""));
    assert!(g.files["versions.tf"].contains("source  = \"hashicorp/azurerm\""));
    assert!(g.files["versions.tf"].contains(">= 1.5.0"));
    assert!(g.files["providers.tf"].contains("features {}"));
    assert_eq!(
        g.manual_steps.len(),
        4,
        "role: 2 partial steps; vm: 1 unconsumed edge (lb / nat links are AWS-only relations); \
         db: no final snapshot on destroy"
    );
}

#[test]
fn manual_node_becomes_variables() {
    let cat = Catalog::builtin();
    let p = example("existing-network.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(!g.files.contains_key("network.tf") || !g.files["network.tf"].contains("aws_vpc"));
    let net = &g.files["network.tf"];
    assert!(net.contains("vpc_id     = var.legacy_id"), "{net}");
    assert!(g.files["variables.tf"].contains("variable \"legacy_id\""));
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(steps.contains("Create Virtual Network \"legacy\" by hand"));
    assert!(steps.contains("`legacy_id`"));
}

#[test]
fn errors_block_export() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    // Remove the resource group -> Azure resources have no ancestor.
    p.remove_entity("rg-1a2b3c4d");
    let err = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap_err();
    assert!(err.to_string().contains("Resource Group container"), "{err}");
    // AWS does not care.
    generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
}

#[test]
fn state_encryption_only_for_opentofu() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    p.settings.state_encryption = true;
    let tofu = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(tofu.files["backend.tf"].contains("encryption {"));
    assert!(tofu.files["variables.tf"].contains("state_passphrase"));
    let tf = generate(&p, &cat, "aws", Tool::Terraform).unwrap();
    assert!(!tf.files.contains_key("backend.tf"));
}

#[test]
fn schema_v2_features() {
    let cat = Catalog::builtin();
    let p = example("three-tier.ttg.json");

    // AWS: no AMI given -> data source + fallback reference; security group rows -> nested
    // ingress/egress blocks; target_type filter attaches the SG but not the bucket.
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let compute = &aws.files["compute.tf"];
    assert!(
        compute.contains("data \"aws_ami\" \"app_server_ubuntu\""),
        "{compute}"
    );
    assert!(
        compute.contains("ami                    = data.aws_ami.app_server_ubuntu.id"),
        "{compute}"
    );
    assert!(compute.contains("vpc_security_group_ids = [\n    aws_security_group.web_sg.id\n  ]"));
    let net = &aws.files["network.tf"];
    assert_eq!(
        net.matches("resource \"aws_vpc_security_group_ingress_rule\"")
            .count(),
        4,
        "web sg: 3, db sg: 1"
    );
    assert_eq!(
        net.matches("resource \"aws_vpc_security_group_egress_rule\"")
            .count(),
        2
    );
    assert!(net.contains("= \"-1\""), "protocol all becomes -1");
    assert!(
        !net.contains("from_port                    = 0"),
        "ports are omitted for protocol all"
    );
    assert_eq!(aws.manual_steps.len(), 1, "bucket link is still a manual step");

    // Azure: one rule resource per row, priorities from item_index, if/else port ranges.
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let net = &az.files["network.tf"];
    assert_eq!(
        net.matches("resource \"azurerm_network_security_rule\"").count(),
        6
    );
    assert!(net.contains("priority                    = 130"));
    assert!(net.contains("destination_port_range      = \"1024-65535\""));
    assert!(net.contains("destination_port_range      = \"*\""));
    assert!(
        az.files["compute.tf"].contains("resource \"azurerm_network_interface_security_group_association\"")
    );

    // With an explicit AMI the data source disappears.
    let mut p2 = p.clone();
    p2.nodes
        .get_mut("vm-7e8f9a0b")
        .unwrap()
        .provider_config
        .get_mut("aws")
        .unwrap()
        .insert("ami".into(), ttg_core::Value::Str("ami-0c1c30571d2dafa9e".into()));
    let aws2 = generate(&p2, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(!aws2.files["compute.tf"].contains("data \"aws_ami\""));
    assert!(aws2.files["compute.tf"].contains("ami                    = \"ami-0c1c30571d2dafa9e\""));
}

#[test]
fn v1_file_cannot_use_v2_features() {
    let bad = r#"
schema_version = 1
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[fields]]
name = "rules"
type = "struct_list"
[[fields.items]]
name = "port"
type = "int"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_thing"
"#;
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let err = Catalog::from_sources([("thing.toml", bad)].into_iter(), [("aws.toml", aws)].into_iter())
        .expect_err("must be rejected");
    assert!(err.to_string().contains("schema_version 2 features"), "{err}");
}

#[test]
fn list_shapes_must_match_the_field_type() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    // `{name}` stands in for the argument source under test.
    let def = |arg: &str| {
        format!(
            r#"
schema_version = 2
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[fields]]
name = "labels"
type = "string_list"
[[fields]]
name = "rules"
type = "struct_list"
[[fields.items]]
name = "port"
type = "int"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_eks_node_group"
[providers.aws.blocks.args]
labels = {arg}
"#
        )
    };
    let reject = |arg: &str, needle: &str| {
        let src = def(arg);
        let err = Catalog::from_sources(
            [("thing.toml", src.as_str())].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .expect_err("must be rejected");
        assert!(err.to_string().contains(needle), "{err}");
    };
    reject(
        r#"{ field = "rules", wrap = "map" }"#,
        "wrap = \"map\" needs a string_list",
    );
    reject(
        r#"{ field = "labels", column = "port" }"#,
        "column needs a struct_list field",
    );
    reject(
        r#"{ field = "rules", column = "missing" }"#,
        "column 'missing' is not an item of 'rules'",
    );
    reject(
        r#"{ field = "rules", column = "port", transform = "kebab" }"#,
        "column cannot be combined with wrap or transform",
    );
    reject(
        r#"{ relation = "attachment", attr = "id", wrap = "map" }"#,
        "wrap = \"map\" is only valid on a field or provider_field",
    );
    // The valid shapes load.
    for arg in [
        r#"{ field = "labels", wrap = "map" }"#,
        r#"{ field = "rules", column = "port" }"#,
    ] {
        let src = def(arg);
        Catalog::from_sources(
            [("thing.toml", src.as_str())].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .unwrap_or_else(|e| panic!("{arg} should load: {e}"));
    }
}

/// `target_shares_ancestor` needs subjects: either a `relation` naming them, or an
/// enclosing `for_each_relation` block whose current target is the subject.
#[test]
fn target_shares_ancestor_needs_a_relation_or_a_repeated_block() {
    let head = r#"
schema_version = 2
[resource]
type = "thing"
category = "network"
display_name = "Thing"
[[relations]]
kind = "sends_to"
targets = ["resource_group"]
cardinality = "optional"
[providers.aws]
[[providers.aws.blocks]]
key = "main"
resource = "aws_thing"
"#;
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let rg = r#"
schema_version = 2
[resource]
type = "resource_group"
category = "organization"
display_name = "Group"
kind = "container"
[providers.aws]
status = "logical"
"#;
    let load = |cond: &str| {
        let def = format!("{head}when = {cond}\n");
        Catalog::from_sources(
            [("thing.toml", def.as_str()), ("resource_group.toml", rg)].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
        .map(|_| ())
    };

    let err = load(r#"{ target_shares_ancestor = "resource_group" }"#).expect_err("no subject");
    assert!(err.to_string().contains("needs a relation"), "{err}");

    let err = load(r#"{ target_shares_ancestor = "resource_group", relation = "reads" }"#)
        .expect_err("undeclared relation");
    assert!(err.to_string().contains("undeclared relation 'reads'"), "{err}");

    load(r#"{ target_shares_ancestor = "resource_group", relation = "sends_to" }"#)
        .expect("a declared relation names its own subjects");
}

#[test]
fn step4_network_database_load_balancer() {
    let cat = Catalog::builtin();
    let p = example("three-tier.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let net = &aws.files["network.tf"];
    assert!(net.contains("resource \"aws_internet_gateway\" \"igw\""));
    assert!(net.contains("resource \"aws_nat_gateway\" \"nat\""));
    assert!(net.contains("allocation_id = aws_eip.nat_eip.id"));
    // one association per attached subnet, via for_each_relation
    assert!(net.contains("resource \"aws_route_table_association\" \"public_routes_assoc_0\""));
    assert!(net.contains("resource \"aws_route_table_association\" \"public_routes_assoc_1\""));
    assert!(net.contains("gateway_id             = aws_internet_gateway.igw.id"));
    assert!(net.contains("nat_gateway_id         = aws_nat_gateway.nat.id"));
    let lb = &aws.files["load_balancer.tf"];
    assert!(lb.contains("load_balancer_type = \"application\""));
    assert!(lb.contains("resource \"aws_lb_target_group_attachment\" \"web_lb_attach_0\""));
    assert!(lb.contains("target_id        = aws_instance.app_server.id"));
    let db = &aws.files["database.tf"];
    assert!(db.contains("resource \"aws_db_subnet_group\" \"app_db_subnets\""));
    assert!(db.contains("password               = var.db_password"));
    assert!(aws.files["variables.tf"].contains("sensitive   = true"));

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let net = &az.files["network.tf"];
    assert!(!net.contains("internet_gateway"), "IGW is logical on Azure");
    assert!(net.contains("next_hop_type       = \"Internet\""));
    assert!(net.contains("resource \"azurerm_subnet_nat_gateway_association\" \"private_routes_nat_0\""));
    assert!(net.contains("resource \"azurerm_subnet_route_table_association\" \"private_routes_assoc_1\""));
    let lb = &az.files["load_balancer.tf"];
    assert!(lb.contains("network_interface_id    = azurerm_network_interface.app_server_nic.id"));
    assert!(lb.contains("public_ip_address_id = azurerm_public_ip.web_lb_pip.id"));
    let db = &az.files["database.tf"];
    assert!(db.contains("resource \"azurerm_postgresql_flexible_server\" \"app_db_pg\""));
    assert!(!db.contains("mysql"));
    assert!(az.files["outputs.tf"].contains("app_db_postgres_fqdn"));
    assert!(
        !az.files["outputs.tf"].contains("mysql_fqdn"),
        "outputs of unplanned blocks are skipped"
    );

    // Switch the engine: the other pair of blocks appears.
    let mut p2 = p.clone();
    p2.nodes
        .get_mut("db-3c4d5e6f")
        .unwrap()
        .config
        .insert("engine".into(), ttg_core::Value::Str("mysql".into()));
    let az2 = generate(&p2, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(az2.files["database.tf"].contains("resource \"azurerm_mysql_flexible_server\" \"app_db_my\""));
    assert!(!az2.files["database.tf"].contains("postgresql"));
}

#[test]
fn platform_example_covers_the_broad_catalog() {
    let cat = Catalog::builtin();
    let p = example("platform.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let f = |name: &str| aws.files.get(name).cloned().unwrap_or_default();
    assert!(f("serverless.tf").contains("resource \"aws_lambda_function\" \"resize\""));
    assert!(f("serverless.tf").contains("filename         = var.resize_package"));
    assert!(f("serverless.tf").contains("resource \"aws_lambda_event_source_mapping\" \"resize_trigger\""));
    assert!(f("container.tf").contains("resource \"aws_eks_node_group\" \"k8s_nodes\""));
    assert!(f("container.tf").contains("aws_iam_role_policy_attachment.k8s_node_policy_cni"));
    assert!(f("dns.tf").contains("format(\"%s.%s\", \"www\", aws_route53_zone.example_zone.name)"));
    assert!(f("secrets.tf").contains("secret_string = var.api_key_value"));
    assert!(f("variables.tf").contains("variable \"api_key_value\""));
    assert!(f("compute.tf").contains("target_group_arns = [\n    aws_lb_target_group.workers_lb_tg.arn\n  ]"));
    assert!(f("database.tf").contains("resource \"aws_dynamodb_table\" \"orders\""));
    assert!(f("database.tf").contains("resource \"aws_elasticache_cluster\" \"sessions\""));
    assert!(f("outputs.tf").contains("aws_elasticache_cluster.sessions.cache_nodes[0].address"));
    assert!(f("monitoring.tf").contains("retention_in_days = 90"));
    // key vault is logical on AWS: nothing named after it is emitted
    assert!(!aws.files.values().any(|c| c.contains("key_vault")));

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let f = |name: &str| az.files.get(name).cloned().unwrap_or_default();
    assert!(f("dns.tf").contains("resource \"azurerm_dns_txt_record\" \"verify_txt\""));
    assert_eq!(
        f("dns.tf").matches("  record {").count(),
        2,
        "one nested record per TXT value"
    );
    assert!(f("dns.tf").contains("record              = one([\"www.example.test\"])"));
    assert!(f("secrets.tf").contains("data \"azurerm_client_config\" \"kv_current\""));
    assert!(f("secrets.tf")
        .contains("tenant_id                  = data.azurerm_client_config.kv_current.tenant_id"));
    assert!(f("secrets.tf").contains("key_vault_id = azurerm_key_vault.kv.id"));
    assert!(f("serverless.tf").contains("python_version = \"3.12\""));
    assert!(f("compute.tf").contains("load_balancer_backend_address_pool_ids = [\n        azurerm_lb_backend_address_pool.workers_lb_pool.id\n      ]"));
    assert!(f("outputs.tf").contains("sensitive   = true"));
    assert!(
        !az.manual_steps
            .iter()
            .any(|m| m.title.contains("Deploy the function code")),
        "code deploy is generated (zip_deploy_file) rather than a manual step"
    );
    assert!(f("serverless.tf").contains("zip_deploy_file"));
    assert!(f("serverless.tf").contains("WEBSITE_RUN_FROM_PACKAGE"));
}

#[test]
fn job_pipeline_wires_functions_to_everything() {
    let cat = Catalog::builtin();
    let p = example("job-pipeline.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let f = |name: &str| aws.files.get(name).cloned().unwrap_or_default();
    let sls = f("serverless.tf");
    // gateway: public URL + send permission + no trigger
    assert!(sls.contains("resource \"aws_lambda_function_url\" \"jobgateway_url\""));
    assert!(sls.contains("QUEUE_URL      = aws_sqs_queue.jobs_queue.url"));
    assert!(sls.contains("Sid = \"QueueSend\""));
    assert!(!sls.contains("\"jobgateway_trigger\""));
    // runner: trigger, VPC, secrets, database env, derived policy
    assert!(sls.contains("resource \"aws_lambda_event_source_mapping\" \"jobrunner_trigger\""));
    assert!(sls.contains("security_group_ids = [\n      aws_security_group.runner_sg.id\n    ]"));
    assert!(sls.contains("DB_HOST        = aws_db_instance.jobs_db.address"));
    assert!(sls.contains("SECRETS        = jsonencode(zipmap("));
    assert!(
        sls.contains("Sid = \"Vpc\"")
            && sls.contains("Sid = \"QueueConsume\"")
            && sls.contains("Sid = \"Secrets\"")
    );
    assert!(!sls.contains("\"jobrunner_policy\"") || !sls.contains("AdministratorAccess"));
    // database takes its password from the secret; URI secret is composed from the database
    assert!(f("database.tf")
        .contains("password               = aws_secretsmanager_secret_version.dbpass_version.secret_string"));
    assert!(f("secrets.tf").contains("secret_string = format(\"postgresql://%s:%s@%s/%s\""));
    // security group rule sourced from another group, no CIDR
    let net = f("network.tf");
    assert!(net.contains("referenced_security_group_id = aws_security_group.runner_sg.id"));
    assert_eq!(aws.manual_steps.len(), 0, "every link is expressed on AWS");

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let f = |name: &str| az.files.get(name).cloned().unwrap_or_default();
    let sls = f("serverless.tf");
    assert!(
        sls.contains("sku_name            = \"EP1\""),
        "VNet-integrated runner needs an elastic plan"
    );
    assert!(
        sls.contains("storage_account_name       = azurerm_storage_account.runner_storage.name"),
        "backing storage comes from the linked node"
    );
    assert!(
        !sls.contains("resource \"azurerm_storage_account\""),
        "no implicit storage accounts"
    );
    let flat = sls.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("AzureWebJobsServiceBus = azurerm_servicebus_namespace.jobs_queue_ns.default_primary_connection_string"));
    assert!(sls.contains("role_definition_name = \"Azure Service Bus Data Sender\""));
    assert!(sls.contains("role_definition_name = \"Key Vault Secrets User\""));
    let db = f("database.tf");
    assert!(db.contains("delegated_subnet_id           = azurerm_subnet.db_a.id"));
    assert!(db.contains("public_network_access_enabled = false"));
    assert!(db.contains("administrator_password        = azurerm_key_vault_secret.dbpass.value"));
    assert!(f("network.tf").contains("Microsoft.DBforPostgreSQL/flexibleServers"));
    assert!(
        !az.manual_steps
            .iter()
            .any(|m| m.title.contains("Open network access")),
        "conditional manual step is skipped when a subnet is linked"
    );
    assert!(
        !az.manual_steps
            .iter()
            .any(|m| m.title.contains("Delegate the database subnet")),
        "delegation is a design-time check now, not a manual step"
    );
    assert!(
        !az.diagnostics
            .iter()
            .any(|d| d.message.contains("delegated to postgres_flexible")),
        "the example's db subnet is delegated"
    );
}

#[test]
fn new_diagnostics_fire() {
    let cat = Catalog::builtin();
    let mut p = example("job-pipeline.ttg.json");
    // Unreferenced log group + duplicate storage account name + too few subnets + open port.
    p.edges.retain(|e| e.relation != ttg_core::Relation::LogsTo);
    p.edges
        .retain(|e| !(e.source == "db-jobs" && e.target == "subnet-d2"));
    p.nodes
        .get_mut("obj-gateway")
        .unwrap()
        .provider_config
        .get_mut("azure")
        .unwrap()
        .insert(
            "account_name".into(),
            ttg_core::Value::Str("jobrunnersax7q2".into()),
        );
    if let Some(ttg_core::Value::Records(rows)) = p.nodes.get_mut("sg-db").unwrap().config.get_mut("rules") {
        rows[0].insert("cidr".into(), ttg_core::Value::Str("0.0.0.0/0".into()));
        rows[0].insert("source_group".into(), ttg_core::Value::Str(String::new()));
    }
    let d = ttg_codegen::diagnostics::run(&p, &cat, "azure");
    let msgs: Vec<String> = d.iter().map(|x| x.message.clone()).collect();
    assert!(
        msgs.iter().any(|m| m.contains("nothing links to this Log Group")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("at least 2 are needed")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("opens port 5432 to the whole internet")),
        "{msgs:?}"
    );
    assert!(
        d.iter()
            .any(|x| x.code == ttg_codegen::Code::DuplicateName && x.severity == Severity::Error),
        "{msgs:?}"
    );
}

#[test]
fn network_checks_fire() {
    use ttg_codegen::diagnostics::run;
    let cat = Catalog::builtin();
    let clean = example("job-pipeline.ttg.json");
    let d = run(&clean, &cat, "aws");
    assert!(!d.iter().any(|x| x.code == ttg_codegen::Code::Network), "{d:?}");

    // Overlapping CIDR, zone outside the region, two route tables on one subnet, and a
    // NAT whose subnet has no internet route.
    let mut p = clean.clone();
    p.nodes
        .get_mut("subnet-a2")
        .unwrap()
        .config
        .insert("cidr_block".into(), ttg_core::Value::Str("10.0.2.0/25".into()));
    p.nodes
        .get_mut("subnet-d1")
        .unwrap()
        .provider_config
        .get_mut("aws")
        .unwrap()
        .insert(
            "availability_zone".into(),
            ttg_core::Value::Str("eu-west-1a".into()),
        );
    p.add_edge("rt-private", "subnet-pub", ttg_core::Relation::Attachment);
    p.edges
        .retain(|e| !(e.source == "rt-public" && e.target == "igw-1"));
    let d = run(&p, &cat, "aws");
    let msgs: Vec<&str> = d
        .iter()
        .filter(|x| x.code == ttg_codegen::Code::Network)
        .map(|x| x.message.as_str())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("overlaps with subnet")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("is not in region eu-west-2")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("a subnet can have only one")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("NAT gateway cannot reach the internet")),
        "{msgs:?}"
    );

    // A function in a subnet with no egress route.
    let mut p = clean.clone();
    p.edges
        .retain(|e| !(e.source == "rt-private" && e.relation == ttg_core::Relation::Attachment));
    let d = run(&p, &cat, "aws");
    assert!(
        d.iter().any(|x| x
            .message
            .contains("no default route via a NAT or Internet Gateway")),
        "{d:?}"
    );

    // A queue drawn inside the network is informational only.
    let mut p = clean.clone();
    p.set_parent("q-jobs", Some("vnet-22222222"));
    let d = run(&p, &cat, "azure");
    assert!(
        d.iter()
            .any(|x| x.severity == Severity::Info && x.message.contains("no network presence")),
        "{d:?}"
    );
}

#[test]
fn reachability_analysis() {
    use ttg_codegen::reach::{analyse, paths_from, Egress, Status};
    let cat = Catalog::builtin();
    let p = example("job-pipeline.ttg.json");
    let r = analyse(&p, &cat, "aws");
    let name = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();

    // Posture: gateway exposed and outside the network; runner leaves via the NAT chain.
    assert_eq!(
        r.posture["fn-gateway"].exposed.as_deref(),
        Some("public HTTPS endpoint")
    );
    assert_eq!(r.posture["fn-gateway"].egress, Egress::Unrestricted);
    match &r.posture["fn-runner"].egress {
        Egress::Via(hops) => {
            let names: Vec<String> = hops.iter().map(|h| name(h)).collect();
            assert_eq!(
                names,
                [
                    "app a",
                    "private routes",
                    "nat",
                    "public a",
                    "public routes",
                    "igw"
                ]
            );
        }
        other => panic!("runner egress: {other:?}"),
    }
    assert_eq!(r.posture["db-jobs"].egress, Egress::NotNeeded);
    assert!(r.posture["db-jobs"].exposed.is_none());

    // Runner: database via the group rule, queue over the API, gateway bucket blocked (no link).
    let paths = paths_from(&p, &cat, &r, "fn-runner");
    let status = |t: &str| paths.iter().find(|x| x.target == t).map(|x| x.status).unwrap();
    assert_eq!(status("db-jobs"), Status::Ok);
    assert_eq!(status("q-jobs"), Status::Ok);
    assert_eq!(status("sec-dbpass"), Status::Ok);
    assert_eq!(status("obj-gateway"), Status::Blocked);
    let db = paths.iter().find(|x| x.target == "db-jobs").unwrap();
    assert!(db.reason.contains("rule from security group"), "{}", db.reason);

    // Gateway: cannot reach the private database, can reach the queue it sends to.
    let paths = paths_from(&p, &cat, &r, "fn-gateway");
    let status = |t: &str| paths.iter().find(|x| x.target == t).map(|x| x.status).unwrap();
    assert_eq!(status("db-jobs"), Status::Blocked);
    assert_eq!(status("q-jobs"), Status::Ok);

    // Break the routing: runner loses its way out, API targets become blocked.
    let mut p2 = p.clone();
    p2.edges
        .retain(|e| !(e.source == "rt-private" && e.relation == ttg_core::Relation::Attachment));
    let r2 = analyse(&p2, &cat, "aws");
    assert!(matches!(r2.posture["fn-runner"].egress, Egress::Blocked(_)));
    let paths = paths_from(&p2, &cat, &r2, "fn-runner");
    assert_eq!(
        paths.iter().find(|x| x.target == "q-jobs").unwrap().status,
        Status::Blocked
    );
    // ... but the in-network database path is unaffected.
    assert_eq!(
        paths.iter().find(|x| x.target == "db-jobs").unwrap().status,
        Status::Ok
    );

    // Open the database rule to a CIDR that does not cover the runner: blocked.
    let mut p3 = p.clone();
    if let Some(ttg_core::Value::Records(rows)) = p3.nodes.get_mut("sg-db").unwrap().config.get_mut("rules") {
        rows[0].insert("source_group".into(), ttg_core::Value::Str(String::new()));
        rows[0].insert("cidr".into(), ttg_core::Value::Str("10.0.99.0/24".into()));
    }
    let r3 = analyse(&p3, &cat, "aws");
    let paths = paths_from(&p3, &cat, &r3, "fn-runner");
    assert_eq!(
        paths.iter().find(|x| x.target == "db-jobs").unwrap().status,
        Status::Blocked
    );

    // Azure: the same database is allowed by default NSG rules when it has no group.
    let mut p4 = p.clone();
    p4.edges
        .retain(|e| !(e.source == "db-jobs" && e.target == "sg-db"));
    let r4 = analyse(&p4, &cat, "azure");
    let paths = paths_from(&p4, &cat, &r4, "fn-runner");
    assert_eq!(
        paths.iter().find(|x| x.target == "db-jobs").unwrap().status,
        Status::Ok
    );
    let r4aws = analyse(&p4, &cat, "aws");
    let paths = paths_from(&p4, &cat, &r4aws, "fn-runner");
    assert_eq!(
        paths.iter().find(|x| x.target == "db-jobs").unwrap().status,
        Status::Unknown
    );
}

#[test]
fn redundant_containment_edges_are_reported_and_ignored() {
    let cat = Catalog::builtin();
    let mut p = example("job-pipeline.ttg.json");
    let (subnet, vnet) = p
        .nodes
        .values()
        .find_map(|n| (n.resource_type == "subnet").then(|| (n.id.clone(), n.parent.clone().unwrap())))
        .unwrap();
    let before = generate(&p, &cat, "aws", Tool::OpenTofu).expect("clean");
    p.add_edge(&subnet, &vnet, ttg_core::Relation::NetworkMembership);
    let edge = p.edges.last().unwrap();
    assert!(ttg_codegen::diagnostics::is_redundant_edge(&p, &cat, edge));
    // Adding it twice does nothing.
    p.add_edge(&subnet, &vnet, ttg_core::Relation::NetworkMembership);
    assert_eq!(
        p.edges
            .iter()
            .filter(|e| e.source == subnet && e.target == vnet)
            .count(),
        1
    );
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    let red: Vec<_> = d
        .iter()
        .filter(|x| x.code == ttg_codegen::Code::Redundant)
        .collect();
    assert_eq!(red.len(), 1, "{d:?}");
    assert_eq!(red[0].severity, Severity::Info);
    // Generated output is unchanged: the explicit edge says nothing new.
    let after = generate(&p, &cat, "aws", Tool::OpenTofu).expect("still clean");
    assert_eq!(before.files, after.files);
}

#[test]
fn topic_fans_out_and_queue_options() {
    let cat = Catalog::builtin();
    let mut p = example("fan-out.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws generates");
    let sv = &g.files["serverless.tf"];
    assert!(sv.contains("resource \"aws_sns_topic\" \"order_events\""));
    assert!(
        sv.contains("protocol             = \"sqs\"") || sv.contains("protocol = \"sqs\""),
        "{sv}"
    );
    assert!(sv.contains("\"aws_sqs_queue_policy\""));
    assert!(sv.contains("\"aws_lambda_permission\""));
    assert!(sv.contains("sns:Publish"));
    assert!(sv.contains("TOPIC_ARN"));
    // FIFO option renames the queue and sets the flags.
    p.nodes
        .get_mut("q-audit")
        .unwrap()
        .provider_config
        .entry("aws".into())
        .or_default()
        .insert("fifo".into(), ttg_core::Value::Bool(true));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("fifo generates");
    let sv = &g.files["serverless.tf"];
    assert!(sv.contains("fifo_queue"), "{sv}");
    assert!(sv.contains("format(\"%s.fifo\""), "{sv}");
    // Azure: Standard namespace for the topic, one subscription per subscriber.
    let g = generate(&p, &cat, "azure", Tool::Terraform).expect("azure generates");
    let sv = &g.files["serverless.tf"];
    assert!(sv.contains("resource \"azurerm_servicebus_topic\""));
    assert_eq!(
        sv.matches("resource \"azurerm_servicebus_subscription\"").count(),
        2,
        "{sv}"
    );
    assert!(sv.contains("TOPIC_CONNECTION"));
}

#[test]
fn provider_scoped_type_is_left_out_elsewhere() {
    let cat = Catalog::builtin();
    let p = example("azure-storage-queue.ttg.json");
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).expect("azure generates");
    let sv = &g.files["serverless.tf"];
    assert!(sv.contains("resource \"azurerm_storage_queue\" \"jobs\""));
    assert!(sv.contains("Storage Queue Data Message Sender"));
    assert!(sv.contains("Storage Queue Data Message Processor"));
    assert!(sv.contains("TRIGGER_STORAGE_QUEUE_NAME"));
    // On AWS the Storage Queue is auto-tagged off the layer: a parity warning, and the
    // export simply leaves it (and the links to it) out.
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    let layer: Vec<_> = d.iter().filter(|x| x.code == ttg_codegen::Code::Layer).collect();
    assert_eq!(layer.len(), 1, "{d:?}");
    assert_eq!(layer[0].severity, Severity::Warning);
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws generates without the queue");
    let sv = &g.files["serverless.tf"];
    assert!(
        !sv.contains("storage_queue") && !sv.contains("STORAGE_QUEUE"),
        "{sv}"
    );
}

#[test]
fn provider_layers_tag_and_namespace_container() {
    let cat = Catalog::builtin();
    let mut p = example("fan-out.ttg.json");
    // Azure: the namespace container is shared, subscriptions forward into the queue.
    let g = generate(&p, &cat, "azure", Tool::Terraform).expect("azure generates");
    let sv = &g.files["serverless.tf"];
    assert_eq!(
        sv.matches("resource \"azurerm_servicebus_namespace\"").count(),
        1,
        "{sv}"
    );
    assert!(sv.contains("forward_to"), "{sv}");
    assert!(sv.contains("TOPIC_CONNECTION"), "{sv}");
    assert!(
        sv.contains("azurerm_servicebus_namespace.orders_bus.default_primary_connection_string"),
        "{sv}"
    );
    // AWS: the container is off-layer (info), its contents are flattened into the RG.
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter()
            .any(|x| x.code == ttg_codegen::Code::Layer && x.severity == Severity::Info),
        "{d:?}"
    );
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws generates");
    assert!(g.files["serverless.tf"].contains("resource \"aws_sns_topic\" \"order_events\""));
    // Tag the audit queue Azure-only: AWS drops it and the topic's subscription to it.
    p.nodes.get_mut("q-audit").unwrap().providers = vec!["azure".into()];
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter()
            .any(|x| x.entity.as_deref() == Some("q-audit") && x.code == ttg_codegen::Code::Layer),
        "{d:?}"
    );
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws generates");
    let sv = &g.files["serverless.tf"];
    assert!(!sv.contains("audit_queue"), "{sv}");
    assert!(!sv.contains("aws_sqs_queue_policy"), "{sv}");
    let g = generate(&p, &cat, "azure", Tool::Terraform).expect("azure still has it");
    assert!(g.files["serverless.tf"].contains("azurerm_servicebus_queue\" \"audit_queue\""));
    // The tag round-trips through the file.
    let text = ttg_core::project::to_string(&p).unwrap();
    assert!(
        text.contains("\"providers\": [\n        \"azure\"\n      ]"),
        "{text}"
    );
    let back = ttg_core::project::load_str(&text).unwrap();
    assert_eq!(back.nodes["q-audit"].providers, vec!["azure".to_string()]);
}

#[test]
fn views_round_trip_and_reach_incoming() {
    let cat = Catalog::builtin();
    let p = example("fan-out.ttg.json");
    assert_eq!(p.views.len(), 1);
    assert!(p.views[0].filter.categories.contains("serverless"));
    let text = ttg_core::project::to_string(&p).unwrap();
    let back = ttg_core::project::load_str(&text).unwrap();
    assert_eq!(back.views, p.views);
    // A view with its own layout, groups and flows round-trips and never touches codegen.
    let jp = example("job-pipeline.ttg.json");
    let v = jp
        .views
        .iter()
        .find(|v| v.name == "Data flow")
        .expect("data-flow view");
    assert!(v.layout.as_ref().is_some_and(|l| !l.positions.is_empty()));
    assert_eq!(v.groups.len(), 4);
    assert_eq!(v.flows.len(), 6);
    assert!(v
        .flows
        .iter()
        .any(|f| matches!(f.from, ttg_core::FlowEnd::Group { .. })));
    // It documents itself: a description, a note, a logical node and numbered steps.
    assert!(!v.description.is_empty());
    assert_eq!(v.notes.len(), 1);
    assert_eq!(v.logicals.len(), 1);
    assert!(v.notes[0].anchor.is_some());
    assert!(v
        .flows
        .iter()
        .any(|f| f.step == Some(1) && matches!(f.from, ttg_core::FlowEnd::Logical { .. })));
    assert_eq!(
        v.flows_in_step_order()
            .iter()
            .filter_map(|f| f.step)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    let mut stripped = jp.clone();
    stripped.views.clear();
    let a = generate(&jp, &cat, "aws", Tool::OpenTofu).unwrap();
    let b = generate(&stripped, &cat, "aws", Tool::OpenTofu).unwrap();
    assert_eq!(a.files, b.files, "views must not change the generated output");
    let t = ttg_core::project::to_string(&jp).unwrap();
    assert_eq!(ttg_core::project::load_str(&t).unwrap().views, jp.views);
    // "Reached by": the audit queue is reached by the publisher only through the topic
    // link, so no direct path; the topic-linked function is reached by nothing that
    // initiates traffic to it.
    let p2 = example("job-pipeline.ttg.json");
    let reach = ttg_codegen::reach::analyse(&p2, &cat, "aws");
    let to_db = ttg_codegen::reach::paths_to(&p2, &cat, &reach, "db-jobs");
    assert!(
        to_db
            .iter()
            .any(|(s, path)| s == "fn-runner" && path.status == ttg_codegen::reach::Status::Ok),
        "{to_db:?}"
    );
    let db = p2.entity("db-jobs").unwrap();
    assert_eq!(ttg_codegen::reach::listening_port(&db), Some(5432));
    assert!(!ttg_codegen::reach::initiates("relational_database"));
}

#[test]
fn extra_arguments_and_native_resources() {
    let mut cat = Catalog::builtin();
    let p = example("native-extras.ttg.json");
    cat.ensure_native_types(&p);
    assert!(cat
        .resource("native:aws:aws_cloudwatch_log_metric_filter")
        .is_some());
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws generates");
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let st = norm(&g.files["storage.tf"]);
    assert!(st.contains("force_destroy = true"), "{st}");
    let nat = norm(&g.files["native.tf"]);
    assert!(nat.contains("metric_transformation {"), "{nat}");
    assert!(
        nat.contains("log_group_name = aws_cloudwatch_log_group.app_logs.name"),
        "{nat}"
    );
    // `$ref` with `block` addresses a secondary block of the target, not its primary one
    // (the bucket versioning resource's id is the bucket name, same as the bucket itself).
    let mut p2 = p.clone();
    let note: ttg_core::Node = serde_json::from_value(serde_json::json!({
        "id": "nat-note", "name": "assets note", "resource_type": "native:aws:aws_s3_bucket_policy",
        "position": {"x": 0, "y": 0}, "parent": "rg-nx000001",
        "extra": {"aws": {"main": {
            "bucket": {"$ref": {"entity": "assets", "block": "versioning", "attr": "id"}},
            "policy": {"$raw": "jsonencode({})"}
        }}}
    }))
    .unwrap();
    p2.nodes.insert(note.id.clone(), note);
    cat.ensure_native_types(&p2);
    let g2 = generate(&p2, &cat, "aws", Tool::OpenTofu).expect("aws generates");
    assert!(
        norm(&g2.files["native.tf"]).contains("bucket = aws_s3_bucket_versioning.assets_versioning.id"),
        "{}",
        g2.files["native.tf"]
    );
    // Azure keeps the curated extras and leaves the AWS-only natives out.
    let g = generate(&p, &cat, "azure", Tool::Terraform).expect("azure generates");
    assert!(norm(&g.files["storage.tf"]).contains("min_tls_version = \"TLS1_2\""));
    assert!(!g.files.contains_key("native.tf"));
    // Schema validation of extras.
    let mut bad = p.clone();
    let m = bad.extra_args_mut("obj-assets", "aws", "main").unwrap();
    m.insert("no_such_argument".into(), serde_json::json!(1));
    m.insert("force_destroy".into(), serde_json::json!(1));
    m.insert("arn".into(), serde_json::json!("x"));
    let d = ttg_codegen::diagnostics::run(&bad, &cat, "aws");
    let msgs: Vec<&str> = d
        .iter()
        .filter(|x| x.code == ttg_codegen::Code::Extra)
        .map(|x| x.message.as_str())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("no argument 'no_such_argument'")),
        "{msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("force_destroy expects a bool")),
        "{msgs:?}"
    );
    assert!(msgs.iter().any(|m| m.contains("arn is read-only")), "{msgs:?}");
    // A native resource missing a required argument is an error.
    let mut bad = p.clone();
    bad.extra_args_mut("nat-metric", "aws", "main")
        .unwrap()
        .remove("pattern");
    let d = ttg_codegen::diagnostics::run(&bad, &cat, "aws");
    assert!(
        d.iter()
            .any(|x| x.severity == Severity::Error && x.message.contains("requires: pattern")),
        "{d:?}"
    );
    // Extras round-trip through the file.
    let text = ttg_core::project::to_string(&p).unwrap();
    let back = ttg_core::project::load_str(&text).unwrap();
    assert_eq!(back.nodes["nat-metric"].extra, p.nodes["nat-metric"].extra);
}

#[test]
fn security_group_name_cannot_start_with_sg_dash_on_aws() {
    let cat = Catalog::builtin();
    let mut p = example("three-tier.ttg.json");
    p.nodes.get_mut("sg-2b3c4d5e").unwrap().name = "sg-web".into();
    let d = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(
        d.iter().any(|x| x.severity == Severity::Error
            && x.message
                .contains("AWS rejects security group names beginning with sg-")),
        "{d:?}"
    );
    // Azure has no such rule, and the original name is unaffected.
    let d = ttg_codegen::diagnostics::run(&p, &cat, "azure");
    assert!(
        !d.iter().any(|x| x.message.contains("beginning with sg-")),
        "{d:?}"
    );
}

#[test]
fn reachability_crosses_peerings_load_balancers_and_private_endpoints() {
    use ttg_codegen::reach::{analyse, paths_from, Status};
    let cat = Catalog::builtin();
    let p = example("hub-spoke.ttg.json");
    let name = |id: &str| p.entity(id).map(|e| e.name.to_string()).unwrap_or_default();
    let hops =
        |path: &ttg_codegen::reach::Path| -> Vec<String> { path.hops.iter().map(|h| name(h)).collect() };

    for provider in ["aws", "azure"] {
        let r = analyse(&p, &cat, provider);
        let paths = paths_from(&p, &cat, &r, "fn-worker");
        let get = |t: &str| paths.iter().find(|x| x.target == t).unwrap();

        // Database in the peered hub: through the peering, allowed by the db group's CIDR rule.
        let db = get("db-hub");
        assert_eq!(db.status, Status::Ok, "{provider}: {}", db.reason);
        assert_eq!(hops(db), ["spoke app", "spoke to hub", "db sg"]);

        // Web instance only accepts the load balancer's group: reached through the LB.
        let web = get("vm-web");
        assert_eq!(web.status, Status::Ok, "{provider}: {}", web.reason);
        assert!(
            web.reason.starts_with("via load balancer \"web lb\""),
            "{}",
            web.reason
        );
        assert!(hops(web).contains(&"web lb".to_string()));

        // Storage over the private endpoint although the spoke has no NAT.
        let obj = get("obj-hub");
        assert_eq!(obj.status, Status::Ok, "{provider}: {}", obj.reason);
        assert_eq!(hops(obj), ["spoke app", "reports endpoint"]);
    }

    // AWS needs routes: unlink the hub route table from the peering and the db path breaks.
    let mut p2 = p.clone();
    p2.edges
        .retain(|e| !(e.source == "peer" && e.target == "rt-hub-app"));
    let r2 = analyse(&p2, &cat, "aws");
    let db = paths_from(&p2, &cat, &r2, "fn-worker")
        .into_iter()
        .find(|x| x.target == "db-hub")
        .unwrap();
    assert_eq!(db.status, Status::Blocked);
    assert!(db.reason.contains("route table of \"jobs db\""), "{}", db.reason);
    // Azure routes across peerings by itself.
    let r2az = analyse(&p2, &cat, "azure");
    assert_eq!(
        paths_from(&p2, &cat, &r2az, "fn-worker")
            .into_iter()
            .find(|x| x.target == "db-hub")
            .unwrap()
            .status,
        Status::Ok
    );

    // No peering at all: blocked with a hint.
    let mut p3 = p.clone();
    p3.remove_entity("peer");
    let r3 = analyse(&p3, &cat, "azure");
    let db = paths_from(&p3, &cat, &r3, "fn-worker")
        .into_iter()
        .find(|x| x.target == "db-hub")
        .unwrap();
    assert_eq!(db.status, Status::Blocked);
    assert!(db.reason.contains("no peering"), "{}", db.reason);

    // Without the private endpoint the storage link needs egress the spoke lacks.
    let mut p4 = p.clone();
    p4.remove_entity("pe-reports");
    let r4 = analyse(&p4, &cat, "aws");
    let obj = paths_from(&p4, &cat, &r4, "fn-worker")
        .into_iter()
        .find(|x| x.target == "obj-hub")
        .unwrap();
    assert_eq!(obj.status, Status::Blocked);
    assert!(obj.reason.contains("no route out"), "{}", obj.reason);

    // Without the LB forwarding link the instance is unreachable, and the note says why.
    let mut p5 = p.clone();
    p5.edges
        .retain(|e| !(e.source == "lb-web" && e.target == "vm-web"));
    let r5 = analyse(&p5, &cat, "aws");
    let web = paths_from(&p5, &cat, &r5, "fn-worker")
        .into_iter()
        .find(|x| x.target == "vm-web")
        .unwrap();
    assert_eq!(web.status, Status::Blocked);

    // Peering routes: one aws_route per linked table, each towards the other network;
    // Azure gets both directions. Provider-scoped relations do not warn elsewhere.
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let g = ttg_codegen::emit::generate(&p, &cat, "aws", ttg_core::Tool::OpenTofu).unwrap();
    let net = norm(&g.files["network.tf"]);
    assert!(net.contains("resource \"aws_vpc_peering_connection\" \"spoke_to_hub\""));
    assert!(net.contains("route_table_id = aws_route_table.spoke_routes.id destination_cidr_block = aws_vpc.hub.cidr_block"), "{net}");
    assert!(net.contains("route_table_id = aws_route_table.hub_app_routes.id destination_cidr_block = aws_vpc.spoke.cidr_block"), "{net}");
    let g = ttg_codegen::emit::generate(&p, &cat, "azure", ttg_core::Tool::OpenTofu).unwrap();
    let net = &g.files["network.tf"];
    assert_eq!(
        net.matches("resource \"azurerm_virtual_network_peering\"")
            .count(),
        2
    );
    assert!(
        !g.diagnostics
            .iter()
            .any(|d| d.message.contains("cannot express 'Attached to'")),
        "{:?}",
        g.diagnostics
    );
    // The spoke worker has no NAT but its only managed link goes over the endpoint.
    assert!(g
        .diagnostics
        .iter()
        .any(|d| d.entity.as_deref() == Some("fn-worker")
            && d.severity == ttg_codegen::diagnostics::Severity::Info
            && d.message.contains("private endpoints")));
}

/// The security posture that used to need `extra` arguments or native resources: one
/// example, three providers, both the key relation and the hardening fields.
#[test]
fn hardened_example_carries_the_posture_on_every_provider() {
    let cat = Catalog::builtin();
    let p = example("hardened.ttg.json");
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");

    // ---- AWS
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let sec = norm(&g.files["security.tf"]);
    assert!(sec.contains("resource \"aws_kms_key\" \"data_key\""), "{sec}");
    assert!(sec.contains("enable_key_rotation = true"), "{sec}");
    assert!(sec.contains("rotation_period_in_days = 90"), "{sec}");
    assert!(sec.contains("deletion_window_in_days = 30"), "{sec}");
    assert!(
        sec.contains("resource \"aws_kms_alias\" \"data_key_alias\""),
        "{sec}"
    );
    // The key policy lets the account administer it and the log / event services use it.
    assert!(sec.contains("Sid = \"AccountAdministration\""), "{sec}");
    assert!(
        sec.contains("format(\"logs.%s.amazonaws.com\", data.aws_region.data_key_region.name)"),
        "{sec}"
    );
    assert!(
        sec.contains("Service = [\"sns.amazonaws.com\", \"sqs.amazonaws.com\""),
        "{sec}"
    );

    // Public access block, TLS-only policy, lifecycle, CORS and access logging.
    let st = norm(&g.files["storage.tf"]);
    assert!(
        st.contains("resource \"aws_s3_bucket_public_access_block\" \"records_store_public_access\""),
        "{st}"
    );
    assert!(st.contains("restrict_public_buckets = true"), "{st}");
    assert!(st.contains("Sid = \"DenyInsecureTransport\""), "{st}");
    assert!(st.contains("\"aws:SecureTransport\" = \"false\""), "{st}");
    assert!(st.contains("expiration { days = 365 }"), "{st}");
    assert!(
        st.contains("noncurrent_version_expiration { noncurrent_days = 30 }"),
        "{st}"
    );
    assert!(
        st.contains("abort_incomplete_multipart_upload { days_after_initiation = 7 }"),
        "{st}"
    );
    assert!(
        st.contains("allowed_origins = [ \"https://app.example.com\" ]"),
        "{st}"
    );
    assert!(
        st.contains("target_bucket = aws_s3_bucket.access_log_store.id"),
        "{st}"
    );
    // The key encrypts the records bucket; the log target keeps SSE-S3 (S3 refuses to
    // deliver logs into a KMS-encrypted bucket) and gets the delivery grant instead.
    assert!(
        st.contains("kms_master_key_id = aws_kms_key.data_key.arn"),
        "{st}"
    );
    assert!(
        !st.contains("\"aws_s3_bucket_server_side_encryption_configuration\" \"access_log_store"),
        "{st}"
    );
    assert!(st.contains("Sid = \"AllowServerAccessLogDelivery\""), "{st}");

    // Database: HA, backups, encryption, a final snapshot named after the server.
    let db = norm(&g.files["database.tf"]);
    assert!(db.contains("multi_az = true"), "{db}");
    assert!(db.contains("backup_retention_period = 14"), "{db}");
    assert!(db.contains("storage_encrypted = true"), "{db}");
    assert!(db.contains("kms_key_id = aws_kms_key.data_key.arn"), "{db}");
    assert!(db.contains("skip_final_snapshot = false"), "{db}");
    assert!(
        db.contains("final_snapshot_identifier = \"records-final\""),
        "{db}"
    );
    assert!(db.contains("performance_insights_enabled = true"), "{db}");
    assert!(db.contains("engine_version = \"16\""), "{db}");

    // Queue, topic and log group all take the same key.
    let srv = norm(&g.files["serverless.tf"]);
    assert!(srv.contains("message_retention_seconds = 1209600"), "{srv}");
    assert!(srv.contains("receive_wait_time_seconds = 20"), "{srv}");
    assert_eq!(
        srv.matches("kms_master_key_id = aws_kms_key.data_key.arn")
            .count(),
        2,
        "queue and topic: {srv}"
    );
    assert!(
        norm(&g.files["monitoring.tf"]).contains("kms_key_id = aws_kms_key.data_key.arn"),
        "log group"
    );

    // Secret: recovery window, key, and a generated value from hashicorp/random.
    let secrets = norm(&g.files["secrets.tf"]);
    assert!(secrets.contains("recovery_window_in_days = 14"), "{secrets}");
    assert!(
        secrets.contains("kms_key_id = aws_kms_key.data_key.arn"),
        "{secrets}"
    );
    assert!(
        secrets.contains("resource \"random_password\" \"db_password_generated\""),
        "{secrets}"
    );
    assert!(
        secrets.contains("secret_string = random_password.db_password_generated.result"),
        "{secrets}"
    );
    assert!(
        g.files["versions.tf"].contains("registry.opentofu.org/hashicorp/random"),
        "the helper provider is required because a random_password is emitted"
    );

    // Registry hardening.
    let reg = norm(&g.files["container.tf"]);
    assert!(reg.contains("image_tag_mutability = \"IMMUTABLE\""), "{reg}");
    assert!(
        reg.contains("image_scanning_configuration { scan_on_push = true }"),
        "{reg}"
    );
    assert!(
        reg.contains("countType = \"imageCountMoreThan\", countNumber = 20"),
        "{reg}"
    );

    // Default tags: one provider-level declaration, not a tag per resource.
    assert!(
        norm(&g.files["providers.tf"])
            .contains("default_tags { tags = { Environment = \"prod\" Project = \"Hardened\" } }"),
        "{}",
        g.files["providers.tf"]
    );

    // ---- Azure
    let g = generate(&p, &cat, "azure", Tool::Terraform).unwrap();
    let st = norm(&g.files["storage.tf"]);
    assert!(st.contains("allow_nested_items_to_be_public = false"), "{st}");
    assert!(st.contains("https_traffic_only_enabled = true"), "{st}");
    assert!(st.contains("min_tls_version = \"TLS1_2\""), "{st}");
    assert!(
        st.contains("delete_after_days_since_modification_greater_than = 365"),
        "{st}"
    );
    assert!(st.contains("delete_after_days_since_creation = 30"), "{st}");
    assert!(st.contains("cors_rule {"), "{st}");
    let db = norm(&g.files["database.tf"]);
    assert!(db.contains("backup_retention_days = 14"), "{db}");
    assert!(
        db.contains("high_availability { mode = \"ZoneRedundant\" }"),
        "{db}"
    );
    assert!(
        norm(&g.files["security.tf"]).contains("resource \"azurerm_key_vault_key\" \"data_key\""),
        "the key lives in the enclosing vault"
    );
    // azurerm has no provider-level default tags: every taggable resource carries them,
    // and a resource's own tags win.
    assert!(!g.files["providers.tf"].contains("default_tags"));
    assert!(
        st.contains("tags = { Environment = \"prod\" Project = \"Hardened\" Name = \"access log store\" }"),
        "{st}"
    );
    assert!(
        norm(&g.files["secrets.tf"]).contains("tags = { Environment = \"prod\" Project = \"Hardened\" }"),
        "a resource with no tags of its own still gets the project's"
    );
    // The links Azure cannot honour are reported at design time, not silently dropped.
    let warned: Vec<&str> = g
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("linked to an Encryption Key"))
        .map(|d| d.entity.as_deref().unwrap_or(""))
        .collect();
    for id in ["obj-data", "db-main", "q-work", "sec-db", "log-app", "tp-events"] {
        assert!(warned.contains(&id), "{id} not warned: {warned:?}");
    }

    // ---- GCP
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let sec = norm(&g.files["security.tf"]);
    assert!(
        sec.contains("resource \"google_kms_key_ring\" \"data_key_ring\""),
        "{sec}"
    );
    assert!(sec.contains("rotation_period = \"7776000s\""), "{sec}");
    let st = norm(&g.files["storage.tf"]);
    assert!(st.contains("public_access_prevention = \"enforced\""), "{st}");
    assert!(
        st.contains("encryption { default_kms_key_name = google_kms_crypto_key.data_key.id }"),
        "{st}"
    );
    assert!(
        st.contains("logging { log_bucket = google_storage_bucket.access_log_store.name"),
        "{st}"
    );
    assert!(st.contains("type = \"AbortIncompleteMultipartUpload\""), "{st}");
    let db = norm(&g.files["database.tf"]);
    assert!(db.contains("availability_type = \"REGIONAL\""), "{db}");
    assert!(db.contains("retained_backups = 14"), "{db}");
    assert!(
        db.contains("encryption_key_name = google_kms_crypto_key.data_key.id"),
        "{db}"
    );
    let srv = norm(&g.files["serverless.tf"]);
    assert!(
        srv.contains("message_retention_duration = format(\"%ds\", 1209600)"),
        "{srv}"
    );
    assert_eq!(
        srv.matches("kms_key_name = google_kms_crypto_key.data_key.id")
            .count(),
        2,
        "queue and topic: {srv}"
    );
    assert!(
        norm(&g.files["monitoring.tf"])
            .contains("cmek_settings { kms_key_name = google_kms_crypto_key.data_key.id }"),
        "log bucket"
    );
    assert!(
        norm(&g.files["secrets.tf"]).contains("customer_managed_encryption {"),
        "a linked key switches Secret Manager to a regional replica: {}",
        g.files["secrets.tf"]
    );
    let reg = norm(&g.files["container.tf"]);
    assert!(reg.contains("docker_config { immutable_tags = true }"), "{reg}");
    assert!(reg.contains("keep_count = 20"), "{reg}");
    // Labels, not tags: Google Cloud only accepts lowercase.
    assert!(
        norm(&g.files["providers.tf"])
            .contains("default_labels = { environment = \"prod\" project = \"hardened\" }"),
        "{}",
        g.files["providers.tf"]
    );
}

/// Nothing emits a `random_*` resource, so the helper provider stays out of versions.tf.
#[test]
fn helper_provider_is_only_required_when_used() {
    let cat = Catalog::builtin();
    let p = example("job-pipeline.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        !g.files["versions.tf"].contains("random"),
        "{}",
        g.files["versions.tf"]
    );
}

/// The safe defaults reach a project written before these fields existed.
#[test]
fn older_projects_gain_the_safe_defaults() {
    let cat = Catalog::builtin();
    let p = example("three-tier.ttg.json");
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let st = norm(&g.files["storage.tf"]);
    assert!(
        st.contains("resource \"aws_s3_bucket_public_access_block\""),
        "{st}"
    );
    assert!(st.contains("Sid = \"DenyInsecureTransport\""), "{st}");
    let db = norm(&g.files["database.tf"]);
    assert!(db.contains("storage_encrypted = true"), "{db}");
    assert!(db.contains("backup_retention_period = 7"), "{db}");
    assert!(db.contains("skip_final_snapshot = false"), "{db}");
    // No project tags configured: no default_tags block at all.
    assert!(
        !g.files["providers.tf"].contains("default_tags"),
        "{}",
        g.files["providers.tf"]
    );
}

/// The Kubernetes story: extra node pools, workload identity, cluster add-ons and logs,
/// and a load balancer that forwards to the cluster.
#[test]
fn kubernetes_example_node_pools_and_workload_identity() {
    let cat = Catalog::builtin();
    let p = example("kubernetes.ttg.json");

    // ------------------------------------------------------------------ AWS
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let c = &aws.files["container.tf"];
    // The GPU pool: its own node group on the cluster's node role, scaling to zero, spot
    // capacity, the NVIDIA image, labels as a map and the taint as a nested block.
    assert!(c.contains("resource \"aws_eks_node_group\" \"gpu\""), "{c}");
    assert!(
        c.contains("node_role_arn   = aws_iam_role.platform_node_role.arn"),
        "{c}"
    );
    assert!(
        c.contains("subnet_ids      = aws_eks_cluster.platform.vpc_config[0].subnet_ids"),
        "the pool inherits the cluster's subnets when none are linked: {c}"
    );
    assert!(c.contains("capacity_type = \"SPOT\""), "{c}");
    assert!(c.contains("ami_type      = \"AL2023_x86_64_NVIDIA\""), "{c}");
    assert!(
        c.contains("labels        = {\n    workload = \"asr\"\n  }"),
        "{c}"
    );
    assert!(
        c.contains("desired_size = 0\n    min_size     = 0\n    max_size     = 4"),
        "{c}"
    );
    assert!(
        c.contains("taint {\n    key    = \"nvidia.com/gpu\"\n    value  = \"present\"\n    effect = \"NO_SCHEDULE\"\n  }"),
        "{c}"
    );
    // Add-ons: one aws_eks_addon per entry of the abstract list.
    assert_eq!(c.matches("resource \"aws_eks_addon\"").count(), 3, "{c}");
    assert!(c.contains("addon_name   = \"eks-pod-identity-agent\""), "{c}");
    assert!(c.contains("addon_name   = \"aws-ebs-csi-driver\""), "{c}");
    // Cluster logs and API endpoint access.
    assert!(c.contains("enabled_cluster_log_types = ["), "{c}");
    assert!(c.contains("endpoint_private_access = false"), "{c}");
    // Workload identity: pod identity association + a least-privilege policy per workload.
    assert!(
        c.contains("resource \"aws_eks_pod_identity_association\" \"api\""),
        "{c}"
    );
    assert!(c.contains("service_account = \"asr-worker\""), "{c}");
    assert!(c.contains("role_arn        = aws_iam_role.api_role.arn"), "{c}");
    assert!(
        c.contains("Sid = \"QueueSend\"") && c.contains("Sid = \"Secrets\""),
        "the api workload's links become policy statements: {c}"
    );
    assert!(
        c.contains("Sid = \"QueueConsume\"") && c.contains("Sid = \"Bucket\""),
        "the asr worker consumes the queue and writes the bucket: {c}"
    );
    // A role pods may assume: EKS Pod Identity principal and sts:TagSession.
    let iam = &aws.files["iam.tf"];
    assert!(
        iam.contains("Action = [\"sts:AssumeRole\", \"sts:TagSession\"]")
            && iam.contains("Service = \"pods.eks.amazonaws.com\""),
        "{iam}"
    );
    // A load balancer forwarding to the cluster registers pod IPs, not instances.
    let lb = &aws.files["load_balancer.tf"];
    assert!(lb.contains("target_type = \"ip\""), "{lb}");
    assert!(!lb.contains("aws_lb_target_group_attachment"), "{lb}");
    assert!(aws
        .manual_steps
        .iter()
        .any(|s| s.title.contains("Bind the target group")));
    assert!(aws.diagnostics.iter().all(|d| d.severity != Severity::Error));

    // ---------------------------------------------------------------- Azure
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let c = &az.files["container.tf"];
    assert!(c.contains("oidc_issuer_enabled       = true"), "{c}");
    assert!(c.contains("workload_identity_enabled = true"), "{c}");
    assert!(
        c.contains("resource \"azurerm_kubernetes_cluster_node_pool\" \"gpu\""),
        "{c}"
    );
    assert!(c.contains("gpu_driver      = \"Install\""), "{c}");
    assert!(c.contains("priority        = \"Spot\""), "{c}");
    assert!(c.contains("min_count             = 0"), "{c}");
    assert!(
        c.contains("node_taints     = formatlist(\"%s=%s:%s\", [\"nvidia.com/gpu\"], [\"present\"], [\"NoSchedule\"])"),
        "struct_list columns become the three lists AKS wants: {c}"
    );
    assert!(
        c.contains("node_labels           = {\n    workload = \"asr\"\n  }"),
        "{c}"
    );
    // The federated credential binds the identity to <ns>/<sa> on the cluster's issuer.
    assert!(
        c.contains("resource \"azurerm_federated_identity_credential\" \"api\""),
        "{c}"
    );
    assert!(
        c.contains("issuer  = azurerm_kubernetes_cluster.platform.oidc_issuer_url"),
        "{c}"
    );
    assert!(
        c.contains("subject = format(\"system:serviceaccount:%s:%s\", \"platform\", \"api\")"),
        "{c}"
    );
    assert!(
        c.contains("role_definition_name = \"Azure Service Bus Data Receiver\""),
        "{c}"
    );
    assert!(
        c.contains("resource \"azurerm_monitor_diagnostic_setting\" \"platform_diag_0\""),
        "{c}"
    );

    // ------------------------------------------------------------------ GCP
    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let c = &gcp.files["container.tf"];
    assert!(
        c.contains("workload_pool = format(\"%s.svc.id.goog\", var.project)"),
        "{c}"
    );
    assert!(
        c.contains("resource \"google_container_node_pool\" \"gpu\""),
        "{c}"
    );
    assert!(c.contains("spot   = true"), "{c}");
    assert!(c.contains("guest_accelerator {"), "{c}");
    assert!(c.contains("gpu_driver_version = \"LATEST\""), "{c}");
    assert!(c.contains("min_node_count = 0"), "{c}");
    assert!(
        c.contains("format(\"serviceAccount:%s[%s/%s]\", google_container_cluster.platform.workload_identity_config[0].workload_pool, \"platform\", \"api\")"),
        "{c}"
    );
    assert!(
        c.contains("role               = \"roles/iam.workloadIdentityUser\""),
        "{c}"
    );
    assert!(c.contains("enable_components = ["), "cluster logging: {c}");

    // A mapping that documents a relation in its own manual step does not also get the
    // generic "link by hand" entry.
    for g in [&az, &gcp] {
        assert!(
            !g.manual_steps.iter().any(|s| s.title.contains("Link \"edge\"")),
            "{:?}",
            g.manual_steps.iter().map(|s| &s.title).collect::<Vec<_>>()
        );
    }
}

// ---------------------------------------------------------------- internet-facing edge

/// Collapse whitespace so assertions can be written on one line whatever the formatter does.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn edge_aws_https_waf_cdn_and_alias() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(g.diagnostics.iter().all(|d| d.severity != Severity::Error));

    // HTTPS listener with the validated certificate, TLS policy and the redirect listener.
    let lb = norm(&g.files["load_balancer.tf"]);
    assert!(lb.contains("protocol = \"HTTPS\""), "{lb}");
    assert!(
        lb.contains("certificate_arn = aws_acm_certificate_validation.site_cert_issued.certificate_arn"),
        "{lb}"
    );
    assert!(lb.contains("ssl_policy = \"ELBSecurityPolicy-TLS13-1-2-2021-06\""));
    assert!(lb.contains("resource \"aws_lb_listener\" \"web_lb_redirect\""));
    assert!(
        lb.contains("redirect { port = \"443\" protocol = \"HTTPS\" status_code = \"HTTP_301\" }"),
        "{lb}"
    );
    // Hardening, and access logs plus the bucket policy the ELB service needs.
    assert!(lb.contains("drop_invalid_header_fields = true"));
    assert!(lb.contains("idle_timeout = 120"));
    assert!(
        lb.contains("access_logs { bucket = aws_s3_bucket.lb_logs.id"),
        "{lb}"
    );
    assert!(lb.contains("resource \"aws_s3_bucket_policy\" \"web_lb_logs_policy\""));
    assert!(lb.contains("logdelivery.elasticloadbalancing.amazonaws.com"));
    assert!(
        lb.contains("depends_on = [ aws_s3_bucket_policy.web_lb_logs_policy ]"),
        "{lb}"
    );

    // ACM with DNS validation: the `raw` + `refs` for_each comprehension names the
    // certificate's own address, and the validation resource waits for every record.
    let net = norm(&g.files["network.tf"]);
    assert!(net.contains("validation_method = \"DNS\""));
    assert!(
        net.contains("for_each = {for o in aws_acm_certificate.site_cert.domain_validation_options : o.domain_name => o}"),
        "{net}"
    );
    assert!(net.contains("name = each.value.resource_record_name"));
    assert!(
        net.contains("validation_record_fqdns = [for r in aws_route53_record.site_cert_validation : r.fqdn]")
    );

    // Web ACL: one managed rule per entry, a rate-based rule, and one association per LB.
    assert!(net.contains("scope = \"REGIONAL\""));
    assert_eq!(
        net.matches("managed_rule_group_statement").count(),
        6,
        "one statement per managed rule group, in the REGIONAL and the CLOUDFRONT ACL"
    );
    assert!(net.contains("override_action { none {} }"), "{net}");
    assert!(
        net.contains("rate_based_statement { limit = 2000 aggregate_key_type = \"IP\""),
        "{net}"
    );
    assert!(net.contains("resource \"aws_wafv2_web_acl_association\" \"edge_waf_assoc_0\""));
    assert!(net.contains("resource_arn = aws_lb.web_lb.arn"));

    // CloudFront over the bucket: origin access control plus the reader policy.
    assert!(net.contains("origin_access_control_id = aws_cloudfront_origin_access_control.assets_cdn_oac.id"));
    assert!(
        net.contains("\"AWS:SourceArn\" = aws_cloudfront_distribution.assets_cdn.arn"),
        "{net}"
    );

    // Alias records: an `alias` block instead of records / ttl, for the load balancer and
    // for the distribution.
    let dns = norm(&g.files["dns.tf"]);
    assert!(
        dns.contains("alias { name = aws_lb.web_lb.dns_name zone_id = aws_lb.web_lb.zone_id evaluate_target_health = false }"),
        "{dns}"
    );
    assert!(
        dns.contains("alias { name = aws_cloudfront_distribution.assets_cdn.domain_name zone_id = aws_cloudfront_distribution.assets_cdn.hosted_zone_id evaluate_target_health = false }"),
        "{dns}"
    );
    assert!(
        !dns.contains("records"),
        "an alias record carries no records: {dns}"
    );
    assert!(!dns.contains("ttl"), "an alias record carries no ttl: {dns}");

    // Cognito: pool, client and hosted domain.
    let iam = norm(&g.files["iam.tf"]);
    assert!(iam.contains("mfa_configuration = \"OPTIONAL\""));
    assert!(iam.contains("password_policy { minimum_length = 12"), "{iam}");
    assert!(iam.contains("resource \"aws_cognito_user_pool_domain\" \"logins_domain\""));
    assert!(g.files["outputs.tf"].contains("output \"logins_client_id\""));
}

#[test]
fn edge_azure_is_honest_about_what_it_cannot_do() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");
    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let net = norm(&g.files["network.tf"]);

    // A certificate drawn inside a Key Vault becomes a vault certificate.
    assert!(net.contains("resource \"azurerm_key_vault_certificate\" \"site_cert\""));
    assert!(net.contains("issuer_parameters { name = \"Self\" }"), "{net}");
    assert!(net.contains("subject = format(\"CN=%s\", \"www.example.com\")"));
    // WAF policy with the OWASP set and the rate-limit custom rule.
    assert!(net.contains("resource \"azurerm_web_application_firewall_policy\" \"edge_waf\""));
    assert!(
        net.contains("managed_rule_set { type = \"OWASP\" version = \"3.2\" }"),
        "{net}"
    );
    assert!(net.contains("rate_limit_threshold = 2000"));
    assert!(net.contains("resource \"azurerm_cdn_endpoint\" \"assets_cdn\""));
    assert!(net.contains("host_name = azurerm_storage_account.assets.primary_blob_host"));

    // The layer-4 load balancer passes 443 through and says so.
    let lb = norm(&g.files["load_balancer.tf"]);
    assert!(lb.contains("frontend_port = 443"));
    assert!(!lb.contains("certificate"), "azurerm_lb terminates nothing: {lb}");

    // An A record aliased to the load balancer targets its public IP.
    assert!(norm(&g.files["dns.tf"]).contains("target_resource_id = azurerm_public_ip.web_lb_pip.id"));

    // Nothing is generated for the user pool, but the logical mapping still explains itself.
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(
        steps.contains("Create the Entra External ID tenant by hand"),
        "{steps}"
    );
    assert!(steps.contains("TLS is not terminated on an Azure Load Balancer"));
    assert!(steps.contains("Attach the policy to an Application Gateway or Front Door"));
}

#[test]
fn edge_gcp_builds_a_global_https_load_balancer() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");
    let g = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let lb = norm(&g.files["load_balancer.tf"]);

    // https swaps the regional passthrough NLB for the global application LB.
    assert!(!lb.contains("google_compute_region_backend_service"), "{lb}");
    assert!(lb.contains("resource \"google_compute_backend_service\" \"web_lb_gbs\""));
    assert!(lb.contains("load_balancing_scheme = \"EXTERNAL_MANAGED\""));
    assert!(lb.contains("resource \"google_compute_url_map\" \"web_lb_urlmap\""));
    assert!(
        lb.contains("ssl_certificates = [ google_compute_managed_ssl_certificate.site_cert.id ]"),
        "{lb}"
    );
    assert!(lb.contains("resource \"google_compute_global_forwarding_rule\" \"web_lb_grule\""));

    let net = norm(&g.files["network.tf"]);
    assert!(
        net.contains(
            "managed { domains = concat([\"www.example.com\"], [\"example.com\", \"assets.example.com\"]) }"
        ),
        "{net}"
    );
    // Cloud Armor: one rule per translatable group, a rate-based ban and the catch-all.
    assert!(net.contains("expression = \"evaluatePreconfiguredWaf('sqli-v33-stable')\""));
    assert!(net.contains("action = \"rate_based_ban\""));
    assert!(net.contains("priority = 2147483647"));
    assert!(net.contains("resource \"google_compute_backend_bucket\" \"assets_cdn\""));
    assert!(net.contains("enable_cdn = true"));

    // Cloud DNS has no alias type: the record holds the global forwarding rule's address.
    assert!(norm(&g.files["dns.tf"])
        .contains("rrdatas = [ google_compute_global_forwarding_rule.web_lb_grule.ip_address ]"));

    // The redirect and the Cloud Armor attachment are reported, not pretended.
    assert!(g.diagnostics.iter().any(|d| d
        .message
        .contains("HTTP-to-HTTPS redirect is not generated on Google Cloud")));
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(
        steps.contains("Attach the policy to the load balancer's backend service"),
        "{steps}"
    );
}

#[test]
fn https_without_a_certificate_is_an_error() {
    let cat = Catalog::builtin();
    let mut p = example("edge.ttg.json");
    p.edges
        .retain(|e| !(e.source == "lb-web" && e.target == "cert-site"));
    for provider in ["aws", "gcp"] {
        let err = generate(&p, &cat, provider, Tool::OpenTofu).unwrap_err();
        assert!(
            err.to_string().contains("no Certificate link"),
            "{provider}: {err}"
        );
    }
    // Azure never terminates TLS on a load balancer, so it has nothing to complain about.
    generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
}

// ---------------------------------------------------------------- provider aliases

#[test]
fn a_cdn_pulls_its_certificate_and_web_acl_into_us_east_1() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();

    // The aliased provider block: the normal arguments with the alias' overrides applied.
    let providers = norm(&g.files["providers.tf"]);
    assert!(
        providers.contains("provider \"aws\" { region = var.region }"),
        "{providers}"
    );
    assert!(
        providers.contains("provider \"aws\" { alias = \"us_east_1\" region = \"us-east-1\" }"),
        "{providers}"
    );

    let net = norm(&g.files["network.tf"]);
    // A second certificate, its validation records and its own validation resource.
    assert!(
        net.contains("resource \"aws_acm_certificate\" \"site_cert_global\" { provider = aws.us_east_1"),
        "{net}"
    );
    assert!(net.contains(
        "for_each = {for o in aws_acm_certificate.site_cert_global.domain_validation_options : o.domain_name => o}"
    ));
    assert!(
        net.contains("resource \"aws_acm_certificate_validation\" \"site_cert_global_issued\" { provider = aws.us_east_1 certificate_arn = aws_acm_certificate.site_cert_global.arn"),
        "{net}"
    );
    // The same rules again, CloudFront-scoped.
    assert!(
        net.contains("resource \"aws_wafv2_web_acl\" \"edge_waf_global\" { provider = aws.us_east_1"),
        "{net}"
    );
    assert!(net.contains("scope = \"CLOUDFRONT\""));
    assert_eq!(
        net.matches("managed_rule_group_statement").count(),
        6,
        "three managed rule groups in each of the two Web ACLs"
    );
    // ... and the distribution uses both copies.
    assert!(
        net.contains("web_acl_id = aws_wafv2_web_acl.edge_waf_global.arn"),
        "{net}"
    );
    assert!(
        net.contains(
            "acm_certificate_arn = aws_acm_certificate_validation.site_cert_global_issued.certificate_arn"
        ),
        "{net}"
    );
    assert!(
        !net.contains("cloudfront_default_certificate"),
        "the distribution has a certificate of its own: {net}"
    );

    // Nothing is left for the operator, and nothing is flagged.
    let steps = &g.files["MANUAL_STEPS.md"];
    assert!(!steps.contains("CLOUDFRONT-scoped Web ACL"), "{steps}");
    assert!(
        !steps.contains("Point the custom domain at the distribution"),
        "{steps}"
    );
    assert!(
        !g.diagnostics.iter().any(|d| d.message.contains("not wired up")),
        "{:?}",
        g.diagnostics
    );
}

#[test]
fn an_alias_nothing_uses_gets_no_provider_block() {
    let cat = Catalog::builtin();
    let mut p = example("edge.ttg.json");
    // Without the CDN's links there is nothing in us-east-1 to configure.
    p.edges
        .retain(|e| !(e.source == "cdn-assets" && (e.target == "cert-site" || e.target == "waf-edge")));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert_eq!(g.files["providers.tf"].matches("provider \"aws\"").count(), 1);
    assert!(!g.files["providers.tf"].contains("alias"));
    let net = &g.files["network.tf"];
    assert!(!net.contains("aws.us_east_1"), "{net}");
    assert!(!net.contains("site_cert_global"), "{net}");
    assert!(!net.contains("edge_waf_global"), "{net}");
    assert!(net.contains("cloudfront_default_certificate = true"));
}

#[test]
fn an_aliased_provider_block_carries_the_project_tags() {
    let cat = Catalog::builtin();
    let mut p = example("edge.ttg.json");
    p.settings.tags.insert("owner".into(), "platform".into());
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let providers = norm(&g.files["providers.tf"]);
    assert_eq!(
        providers
            .matches("default_tags { tags = { owner = \"platform\" } }")
            .count(),
        2,
        "both configurations tag what they create: {providers}"
    );
}

// ---------------------------------------------------------------- DNS alias to a CDN

#[test]
fn a_record_aliases_a_cdn_on_every_provider() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");

    // AWS: a Route 53 alias block, no records and no ttl.
    let aws = norm(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["dns.tf"]);
    assert!(
        aws.contains("name = aws_cloudfront_distribution.assets_cdn.domain_name"),
        "{aws}"
    );

    // Azure: the endpoint is the alias target of the A record set.
    let azure = norm(&generate(&p, &cat, "azure", Tool::OpenTofu).unwrap().files["dns.tf"]);
    assert!(
        azure.contains("resource \"azurerm_dns_a_record\" \"assets_record_a\""),
        "{azure}"
    );
    assert!(
        azure.contains("target_resource_id = azurerm_cdn_endpoint.assets_cdn.id"),
        "{azure}"
    );
    assert!(
        !azure.contains("records = ["),
        "an alias record has no literal values: {azure}"
    );

    // Google Cloud has no alias record, so the CDN reserves an address for the record.
    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    assert!(
        norm(&gcp.files["network.tf"]).contains(
            "resource \"google_compute_global_address\" \"assets_cdn_addr\" { name = \"assets-cdn-address\""
        ),
        "{}",
        gcp.files["network.tf"]
    );
    assert!(norm(&gcp.files["dns.tf"])
        .contains("rrdatas = [ google_compute_global_address.assets_cdn_addr.address ]"));
    assert!(gcp.files["MANUAL_STEPS.md"].contains("Give the reserved address to the front end"));
}

#[test]
fn a_cdn_alias_needs_no_placeholder_values() {
    let cat = Catalog::builtin();
    let p = example("edge.ttg.json");
    for provider in ["aws", "azure", "gcp"] {
        let g = generate(&p, &cat, provider, Tool::OpenTofu).unwrap();
        assert!(
            !g.diagnostics.iter().any(|d| d.message.contains("has no values")),
            "{provider}: {:?}",
            g.diagnostics
        );
    }
}

#[test]
fn azure_accepts_a_cname_alias_of_a_cdn_but_not_of_a_load_balancer() {
    let cat = Catalog::builtin();
    let mut p = example("edge.ttg.json");
    p.nodes
        .get_mut("rec-assets")
        .unwrap()
        .config
        .insert("record_type".into(), ttg_core::Value::Str("CNAME".into()));

    let g = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let dns = norm(&g.files["dns.tf"]);
    assert!(
        dns.contains("resource \"azurerm_dns_cname_record\" \"assets_record_cname\""),
        "{dns}"
    );
    assert!(
        dns.contains("target_resource_id = azurerm_cdn_endpoint.assets_cdn.id"),
        "{dns}"
    );
    assert!(
        !dns.contains("record = one("),
        "an alias CNAME has no literal record: {dns}"
    );

    // A load balancer alias is its public IP address, which no CNAME can name.
    p.edges
        .retain(|e| !(e.source == "rec-assets" && e.target == "cdn-assets"));
    p.add_edge("rec-assets", "lb-web", ttg_core::Relation::AttributeReference);
    let err = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap_err();
    assert!(err.to_string().contains("set the type to A"), "{err}");
}

// ---------------------------------------------------------------------------
// Operational rest: dead-letter queues, file systems, budgets, targetless
// endpoints, repositories, alarm presets, topic subscriptions, flow logs,
// audit trails. All against examples/operations.ttg.json.
// ---------------------------------------------------------------------------

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn dead_letter_queues_on_every_provider() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        squash(&aws.files["serverless.tf"]).contains(
            "redrive_policy = jsonencode({ deadLetterTargetArn = aws_sqs_queue.transcode_dead_letters.arn, maxReceiveCount = 5 })"
        ),
        "{}",
        aws.files["serverless.tf"]
    );

    // Both queues sit in one Service Bus Namespace, so the dead letters are forwarded.
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let sb = squash(&az.files["serverless.tf"]);
    assert!(sb.contains("dead_lettering_on_message_expiration = true"), "{sb}");
    assert!(sb.contains("max_delivery_count = 5"), "{sb}");
    assert!(
        sb.contains(
            "forward_dead_lettered_messages_to = azurerm_servicebus_queue.transcode_dead_letters.name"
        ),
        "{sb}"
    );

    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let sl = squash(&gcp.files["serverless.tf"]);
    assert!(
        sl.contains(
            "dead_letter_policy { dead_letter_topic = google_pubsub_topic.transcode_dead_letters.id \
             max_delivery_attempts = 5 }"
        ),
        "{sl}"
    );
    assert!(sl.contains("resource \"google_pubsub_topic_iam_member\""), "{sl}");
    assert!(
        sl.contains("resource \"google_pubsub_subscription_iam_member\""),
        "{sl}"
    );
    assert!(
        sl.contains("data \"google_project\""),
        "the service agent is named after the project number: {sl}"
    );
}

/// A dead-letter queue in a namespace of its own cannot be forwarded into.
#[test]
fn dead_letters_across_namespaces_are_reported() {
    let cat = Catalog::builtin();
    let mut p = example("operations.ttg.json");
    let q = p.nodes.get_mut("q-dead").unwrap();
    q.parent = Some("rg-ops".into());
    q.provider_config.entry("azure".into()).or_default().insert(
        "namespace_name".into(),
        ttg_core::Value::Str("ttg-operations-dlq".into()),
    );
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(
        !az.files["serverless.tf"].contains("forward_dead_lettered_messages_to"),
        "{}",
        az.files["serverless.tf"]
    );
    assert!(
        az.diagnostics.iter().any(|d| d
            .message
            .contains("only auto-forwards dead letters inside one namespace")),
        "{:?}",
        az.diagnostics
    );
}

#[test]
fn file_system_gets_a_mount_target_per_subnet() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let st = &aws.files["storage.tf"];
    assert!(
        st.contains("resource \"aws_efs_file_system\" \"shared_media\""),
        "{st}"
    );
    assert_eq!(
        st.matches("resource \"aws_efs_mount_target\"").count(),
        2,
        "one per subnet: {st}"
    );
    assert!(
        squash(st).contains("security_groups = [ aws_security_group.nfs_clients.id ]"),
        "{st}"
    );
    assert!(aws.files["outputs.tf"].contains("shared_media_mount_targets"));

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let st = squash(&az.files["storage.tf"]);
    assert!(st.contains("account_kind = \"FileStorage\""), "{st}");
    assert!(st.contains("enabled_protocol = \"NFS\""), "{st}");

    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let st = squash(&gcp.files["storage.tf"]);
    assert!(st.contains("resource \"google_filestore_instance\""), "{st}");
    assert!(st.contains("tier = \"BASIC_HDD\""), "{st}");
    assert!(st.contains("network = google_compute_network.core.name"), "{st}");
}

#[test]
fn budget_notifies_at_a_percentage_of_the_limit() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(aws.contains("budget_type = \"COST\""), "{aws}");
    assert!(aws.contains("time_unit = \"MONTHLY\""), "{aws}");
    assert!(
        aws.contains("threshold = 80 threshold_type = \"PERCENTAGE\""),
        "{aws}"
    );

    // A pinned start month avoids the timestamp()-derived default.
    let az = squash(&generate(&p, &cat, "azure", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        az.contains("time_period { start_date = \"2026-01-01T00:00:00Z\" }"),
        "{az}"
    );

    let gcp = squash(&generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        gcp.contains("billing_account = \"012345-6789AB-CDEF01\""),
        "{gcp}"
    );
    assert!(
        gcp.contains("threshold_rules { threshold_percent = 0.8 }"),
        "{gcp}"
    );
}

#[test]
fn budget_without_a_start_month_computes_one() {
    let cat = Catalog::builtin();
    let mut p = example("operations.ttg.json");
    p.nodes
        .get_mut("bud-monthly")
        .unwrap()
        .provider_config
        .get_mut("azure")
        .unwrap()
        .remove("start_month");
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(
        squash(&az.files["monitoring.tf"])
            .contains("start_date = formatdate(\"YYYY-MM-01'T'00:00:00Z\", timestamp())"),
        "{}",
        az.files["monitoring.tf"]
    );
    assert!(az
        .manual_steps
        .iter()
        .any(|s| s.title.contains("Pin the budget's start month")));
}

#[test]
fn aws_endpoints_need_no_target_and_gateways_take_route_tables() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let net = squash(&aws.files["network.tf"]);
    assert!(
        net.contains("vpc_endpoint_type = \"Gateway\" route_table_ids = [ aws_route_table.app_routes.id ]"),
        "{net}"
    );
    assert!(
        !net.contains("subnet_ids = [ ]"),
        "a gateway endpoint has no subnets: {net}"
    );
    assert!(
        net.contains(
            "service_name = \"com.amazonaws.eu-west-2.logs\" vpc_endpoint_type = \"Interface\" \
             subnet_ids = [ aws_subnet.apps_a.id, aws_subnet.apps_b.id ]"
        ),
        "one interface per zone, and no 'Connects to' link at all: {net}"
    );
}

/// Azure still insists on the link AWS can do without.
#[test]
fn azure_private_endpoint_still_needs_its_target() {
    let cat = Catalog::builtin();
    let mut p = example("hub-spoke.ttg.json");
    p.edges
        .retain(|e| !(e.source == "pe-reports" && e.relation == ttg_core::Relation::AttributeReference));
    let err = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("needs a 'Connects to' link"), "{msg}");
    // AWS is happy: the service name is enough.
    generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
}

#[test]
fn a_registry_can_hold_several_repositories() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let c = &aws.files["container.tf"];
    assert_eq!(c.matches("resource \"aws_ecr_repository\"").count(), 3, "{c}");
    assert!(c.contains("name                 = \"worker\""), "{c}");
    assert_eq!(
        c.matches("resource \"aws_ecr_lifecycle_policy\"").count(),
        3,
        "{c}"
    );
    assert!(
        squash(c).contains("image_tag_mutability = \"IMMUTABLE\""),
        "the posture applies to each: {c}"
    );
    assert!(
        squash(&aws.files["outputs.tf"]).contains(
            "value = [ aws_ecr_repository.service_images_repo_0.repository_url, \
             aws_ecr_repository.service_images_repo_1.repository_url, \
             aws_ecr_repository.service_images_repo_2.repository_url ]"
        ),
        "{}",
        aws.files["outputs.tf"]
    );

    // With no list the registry node is itself the one repository.
    let mut p2 = p.clone();
    p2.nodes
        .get_mut("reg-images")
        .unwrap()
        .config
        .remove("repositories");
    let aws2 = generate(&p2, &cat, "aws", Tool::OpenTofu).unwrap();
    let c2 = &aws2.files["container.tf"];
    assert_eq!(c2.matches("resource \"aws_ecr_repository\"").count(), 1, "{c2}");
    assert!(c2.contains("\"aws_ecr_repository\" \"service_images\""), "{c2}");
}

#[test]
fn alarm_presets_fill_in_the_provider_metric() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        aws.contains("metric_name = \"ApproximateNumberOfMessagesVisible\""),
        "{aws}"
    );
    assert!(aws.contains("statistic = \"Maximum\""), "{aws}");
    assert!(aws.contains("namespace = \"AWS/SQS\""), "{aws}");
    assert!(
        aws.contains("dimensions = { QueueName = aws_sqs_queue.transcode_dead_letters.name }"),
        "{aws}"
    );

    let az = squash(&generate(&p, &cat, "azure", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        az.contains("metric_namespace = \"Microsoft.ServiceBus/namespaces\""),
        "{az}"
    );
    assert!(az.contains("metric_name = \"DeadletteredMessages\""), "{az}");

    let gcp = squash(&generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        gcp.contains("pubsub.googleapis.com/subscription/num_undelivered_messages"),
        "{gcp}"
    );
}

/// A preset the watched type does not have is refused rather than guessed at.
#[test]
fn an_alarm_on_a_metric_the_target_lacks_is_an_error() {
    let cat = Catalog::builtin();
    let mut p = example("operations.ttg.json");
    let set = |p: &mut ttg_core::Project, v: &str| {
        p.nodes
            .get_mut("alm-dlq")
            .unwrap()
            .config
            .insert("metric".into(), ttg_core::Value::Str(v.into()));
    };
    set(&mut p, "lb_5xx");
    let err = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_err();
    assert!(
        format!("{err:?}").contains("CloudWatch does not publish that metric"),
        "{err:?}"
    );

    // 'custom' hands back the free-text fields, whose declared defaults still apply.
    set(&mut p, "custom");
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(aws.files["monitoring.tf"].contains("metric_name = \"CPUUtilization\""));
    p.nodes
        .get_mut("alm-dlq")
        .unwrap()
        .provider_config
        .entry("aws".into())
        .or_default()
        .insert(
            "metric_name".into(),
            ttg_core::Value::Str("MessagesSpilled".into()),
        );
    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(aws.files["monitoring.tf"].contains("metric_name = \"MessagesSpilled\""));
}

#[test]
fn topic_subscriptions_reach_people_where_they_can() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["serverless.tf"]);
    assert!(
        aws.contains("resource \"aws_sns_topic_subscription\" \"ops_alerts_sub_endpoint_0\""),
        "{aws}"
    );
    assert!(
        aws.contains("protocol = \"email\" endpoint = \"ops@example.com\""),
        "{aws}"
    );

    for provider in ["azure", "gcp"] {
        let g = generate(&p, &cat, provider, Tool::OpenTofu).unwrap();
        assert!(
            g.diagnostics.iter().any(|d| {
                d.message.contains("ops@example.com") || d.message.contains("endpoint subscriptions")
            }),
            "{provider} should say it cannot mail anyone: {:?}",
            g.diagnostics
        );
    }

    // https is the one protocol Pub/Sub can push to.
    let mut p2 = p.clone();
    let mut row = ttg_core::Record::new();
    row.insert("protocol".into(), ttg_core::Value::Str("https".into()));
    row.insert(
        "endpoint".into(),
        ttg_core::Value::Str("https://ops.example.com/hook".into()),
    );
    p2.nodes
        .get_mut("tp-alerts")
        .unwrap()
        .config
        .insert("subscriptions".into(), ttg_core::Value::Records(vec![row]));
    let gcp = squash(&generate(&p2, &cat, "gcp", Tool::OpenTofu).unwrap().files["serverless.tf"]);
    assert!(
        gcp.contains("push_config { push_endpoint = \"https://ops.example.com/hook\" }"),
        "{gcp}"
    );
}

#[test]
fn flow_logs_take_the_shape_each_provider_gives_them() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = squash(&generate(&p, &cat, "aws", Tool::OpenTofu).unwrap().files["network.tf"]);
    assert!(aws.contains("resource \"aws_flow_log\" \"core_flow\""), "{aws}");
    assert!(aws.contains("traffic_type = \"ALL\""), "{aws}");
    assert!(aws.contains("Service = \"vpc-flow-logs.amazonaws.com\""), "{aws}");
    assert!(
        aws.contains("iam_role_arn = aws_iam_role.core_flow_role.arn"),
        "{aws}"
    );

    // Google Cloud logs flows per subnet, from the network's own setting.
    let gcp = squash(&generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap().files["network.tf"]);
    assert_eq!(gcp.matches("enable_flow_logs = true").count(), 2, "{gcp}");
    assert_eq!(
        gcp.matches("aggregation_interval = \"INTERVAL_5_SEC\"").count(),
        2
    );

    // Azure needs a Network Watcher and a storage account, so it says so.
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(az
        .manual_steps
        .iter()
        .any(|s| s.title.contains("Network Watcher")));

    // With flow logs off the whole apparatus disappears.
    let mut p2 = p.clone();
    p2.containers
        .get_mut("vnet-ops")
        .unwrap()
        .config
        .insert("flow_logs".into(), ttg_core::Value::Bool(false));
    let aws2 = generate(&p2, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(!aws2.files["network.tf"].contains("aws_flow_log"));
    let gcp2 = generate(&p2, &cat, "gcp", Tool::OpenTofu).unwrap();
    assert!(!gcp2.files["network.tf"].contains("log_config"));
}

#[test]
fn an_audit_trail_brings_its_bucket_policy_with_it() {
    let cat = Catalog::builtin();
    let p = example("operations.ttg.json");

    let aws = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let mon = squash(&aws.files["monitoring.tf"]);
    assert!(
        mon.contains("resource \"aws_cloudtrail\" \"account_activity\""),
        "{mon}"
    );
    assert!(
        mon.contains("s3_bucket_name = aws_s3_bucket.audit_archive.id"),
        "{mon}"
    );
    assert!(
        mon.contains("depends_on = [ aws_s3_bucket_policy.audit_archive_policy ]"),
        "CloudTrail is created only once the bucket policy lets it write: {mon}"
    );
    let st = squash(&aws.files["storage.tf"]);
    assert!(st.contains("Sid = \"AWSCloudTrailAclCheck\""), "{st}");
    assert!(st.contains("Sid = \"AWSCloudTrailWrite\""), "{st}");

    // Azure records the Activity Log without being asked; Google Cloud configures audit logs.
    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    assert!(
        !az.files
            .values()
            .any(|f| f.contains("cloudtrail") || f.contains("audit_log")),
        "nothing is emitted on Azure"
    );
    let gcp = squash(&generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(gcp.contains("service = \"allServices\""), "{gcp}");
    assert!(
        gcp.contains("audit_log_config { log_type = \"ADMIN_READ\" }"),
        "{gcp}"
    );
    assert!(
        !gcp.contains("DATA_READ"),
        "data access logging is off by default: {gcp}"
    );

    let mut p2 = p.clone();
    p2.nodes
        .get_mut("trail-account")
        .unwrap()
        .config
        .insert("data_events".into(), ttg_core::Value::Bool(true));
    let gcp2 = squash(&generate(&p2, &cat, "gcp", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(gcp2.contains("log_type = \"DATA_READ\""), "{gcp2}");
    assert!(gcp2.contains("log_type = \"DATA_WRITE\""), "{gcp2}");
    let aws2 = squash(&generate(&p2, &cat, "aws", Tool::OpenTofu).unwrap().files["monitoring.tf"]);
    assert!(
        aws2.contains("event_selector { read_write_type = \"All\" include_management_events = true }"),
        "{aws2}"
    );
}

/// A trail with nowhere to write is refused rather than exported half-done.
#[test]
fn an_audit_trail_without_a_bucket_is_an_error() {
    let cat = Catalog::builtin();
    let mut p = example("operations.ttg.json");
    p.edges.retain(|e| e.source != "trail-account");
    let err = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap_err();
    assert!(
        format!("{err:?}").contains("CloudTrail delivers into one"),
        "{err:?}"
    );
    // Azure and Google Cloud need no bucket at all.
    generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
}

/// R2.10: a check with `severity = "omit"` leaves one entity out of one provider's
/// export instead of blocking the whole export. The alarm below watches a queue's oldest
/// message age, which Service Bus does not report: AWS and Google Cloud generate it,
/// Azure warns and generates everything else.
#[test]
fn an_omit_check_leaves_the_entity_out_of_that_provider() {
    let cat = Catalog::builtin();
    let mut p = example("operations.ttg.json");
    p.nodes.get_mut("alm-dlq").unwrap().config.insert(
        "metric".into(),
        ttg_core::Value::Str("queue_oldest_message_age".into()),
    );

    // AWS and Google Cloud have the metric: the alarm is generated, nothing is said.
    for provider in ["aws", "gcp"] {
        let g = generate(&p, &cat, provider, Tool::OpenTofu).expect("exports");
        assert!(
            g.files["monitoring.tf"].contains("dead_letters_piling_up"),
            "{provider}: {}",
            g.files["monitoring.tf"]
        );
    }

    // Azure: the export succeeds, the alarm is simply not in it.
    let d = ttg_codegen::diagnostics::run(&p, &cat, "azure");
    let omitted: Vec<_> = d
        .iter()
        .filter(|x| x.entity.as_deref() == Some("alm-dlq") && x.code == ttg_codegen::Code::Check)
        .collect();
    assert_eq!(omitted.len(), 1, "reported once, not once per pass: {d:?}");
    assert_eq!(omitted[0].severity, Severity::Warning);
    assert!(
        omitted[0]
            .message
            .contains("Azure Monitor has no platform metric")
            && omitted[0]
                .message
                .ends_with("; left out of the Microsoft Azure export"),
        "{:?}",
        omitted[0]
    );
    assert!(d.iter().all(|x| x.severity != Severity::Error), "{d:?}");

    let g = generate(&p, &cat, "azure", Tool::Terraform).expect("azure still exports");
    let all = g.files.values().cloned().collect::<Vec<_>>().join("\n");
    assert!(!all.contains("dead_letters_piling_up"), "{all}");
    assert!(!all.contains("azurerm_monitor_metric_alert"), "{all}");
    // Nothing the alarm pointed at is broken: the queue it watched is still exported and
    // no reference to the alarm is left dangling.
    assert!(all.contains("azurerm_servicebus_queue"), "{all}");
    assert!(!all.contains("alm_dlq") && !all.contains("alm-dlq"), "{all}");

    // The layer itself no longer holds the alarm or the link to the queue it watched.
    let layer = ttg_codegen::layers::project_for(&p, &cat, "azure");
    assert!(!layer.nodes.contains_key("alm-dlq"));
    assert!(layer
        .edges
        .iter()
        .all(|e| e.source != "alm-dlq" && e.target != "alm-dlq"));

    // Views: the alarm still draws (it is in the project), but a `providers = ["azure"]`
    // filter treats it as off that layer, the same as a tagged entity.
    let filter = ttg_core::ViewFilter {
        providers: ["azure".to_string()].into(),
        ..Default::default()
    };
    let vis = ttg_codegen::views::visible_set(&p, &cat, &filter).expect("filtered");
    assert!(!vis.contains("alm-dlq"), "{vis:?}");
    assert!(vis.contains("q-dead"), "{vis:?}");
    let filter = ttg_core::ViewFilter {
        providers: ["aws".to_string()].into(),
        ..Default::default()
    };
    let vis = ttg_codegen::views::visible_set(&p, &cat, &filter).expect("filtered");
    assert!(vis.contains("alm-dlq"), "{vis:?}");
}

/// R2.17: while the target is AWS, the errors the other providers would raise are
/// reported as warnings, so "this will block the Azure export" is visible before anyone
/// switches the target.
#[test]
fn other_providers_errors_are_reported_as_warnings() {
    let cat = Catalog::builtin();
    let mut p = example("edge.ttg.json");
    // A certificate outside a Key Vault: Azure has nowhere to put it.
    p.nodes.get_mut("cert-site").unwrap().parent = Some("rg-edge".into());

    let aws = ttg_codegen::diagnostics::run(&p, &cat, "aws");
    assert!(aws.iter().all(|d| d.severity != Severity::Error), "{aws:?}");
    assert!(aws.iter().all(|d| d.provider.is_none()), "{aws:?}");

    let others = ttg_codegen::diagnostics::other_providers(&p, &cat, "aws");
    let azure: Vec<_> = others
        .iter()
        .filter(|d| d.provider.as_deref() == Some("azure") && d.entity.as_deref() == Some("cert-site"))
        .collect();
    assert_eq!(azure.len(), 1, "{others:?}");
    assert_eq!(azure[0].severity, Severity::Warning);
    assert!(
        azure[0].message.starts_with("[Microsoft Azure] ")
            && azure[0]
                .message
                .ends_with("(would block the Microsoft Azure export)"),
        "{:?}",
        azure[0]
    );
    assert!(
        others.iter().all(|d| d.severity == Severity::Warning),
        "{others:?}"
    );
    // `run_all` is the two lists together, and the AWS export is not blocked by them.
    let all = ttg_codegen::diagnostics::run_all(&p, &cat, "aws");
    assert_eq!(all.len(), aws.len() + others.len());
    generate(&p, &cat, "aws", Tool::OpenTofu).expect("aws still exports");

    // With Azure as the target it is the target's own error, reported once, and nothing
    // is repeated in the other-provider list.
    let az = ttg_codegen::diagnostics::run_all(&p, &cat, "azure");
    let mine: Vec<_> = az
        .iter()
        .filter(|d| d.entity.as_deref() == Some("cert-site") && d.severity == Severity::Error)
        .collect();
    assert_eq!(mine.len(), 1, "{az:?}");
    assert!(mine[0].provider.is_none());
    assert!(
        az.iter()
            .all(|d| d.provider.as_deref() != Some("azure") && !d.message.contains("[Microsoft Azure]")),
        "{az:?}"
    );
    // An entity tagged for one provider produces nothing for the others.
    p.nodes.get_mut("cert-site").unwrap().providers = vec!["aws".into()];
    let others = ttg_codegen::diagnostics::other_providers(&p, &cat, "aws");
    assert!(
        others.iter().all(|d| d.entity.as_deref() != Some("cert-site")),
        "{others:?}"
    );
}
