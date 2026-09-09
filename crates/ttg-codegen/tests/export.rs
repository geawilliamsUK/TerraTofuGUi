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
        6,
        "role: 2 partial steps; db: 1 (delegate subnet); lb, nat, vm: 1 unconsumed edge each"
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
    assert!(az
        .manual_steps
        .iter()
        .any(|m| m.title.contains("Deploy the function code")));
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
    assert!(sls.contains("AzureWebJobsServiceBus = azurerm_servicebus_namespace.jobs_queue_ns.default_primary_connection_string"));
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
    assert!(az
        .manual_steps
        .iter()
        .any(|m| m.title.contains("Delegate the database subnet")));
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
    assert_eq!(v.flows.len(), 5);
    assert!(v
        .flows
        .iter()
        .any(|f| matches!(f.from, ttg_core::FlowEnd::Group { .. })));
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
