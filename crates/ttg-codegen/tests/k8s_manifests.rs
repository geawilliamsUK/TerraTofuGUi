//! The Kubernetes manifests export (`settings.kubernetes_manifests`, `ttg export --k8s`):
//! what `examples/kubernetes.ttg.json` produces for each provider, the manual steps it
//! replaces, the outputs it reads, determinism, and two checks against real tools:
//!
//! * `kubectl annotate --local -f <dir> -o json` parses every rendered document with
//!   kubectl's own decoder and needs no cluster (`apply --dry-run=client` does: it asks the
//!   API server for its resource list first). Skips when kubectl is missing unless
//!   `TTG_REQUIRE_KUBECTL` is set.
//! * `render.sh` (bash) and `render.ps1` (PowerShell) are run against a stand-in for
//!   `tofu output` and must produce exactly what `k8s::render_tokens` does. Each skips
//!   when its shell is missing.

use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use ttg_catalog::Catalog;
use ttg_codegen::{generate, k8s, Generated};
use ttg_core::Tool;

fn example() -> ttg_core::Project {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/kubernetes.ttg.json");
    let p = ttg_core::project::load(&p).expect("example loads");
    assert!(
        p.settings.kubernetes_manifests,
        "the Kubernetes example is saved with its manifests on"
    );
    p
}

fn titles(g: &Generated) -> Vec<String> {
    g.manual_steps.iter().map(|s| s.title.clone()).collect()
}

/// The `k8s/*.yaml` files of a generated export.
fn manifests(g: &Generated) -> Vec<(&String, &String)> {
    g.files
        .iter()
        .filter(|(n, _)| n.starts_with("k8s/") && n.ends_with(".yaml"))
        .collect()
}

/// A stand-in value for every token, one of them with the two characters a
/// double-quoted YAML string has to escape.
fn dummy_values(g: &Generated) -> IndexMap<String, String> {
    let mut names: Vec<String> = Vec::new();
    for (_, text) in manifests(g) {
        for t in k8s::tokens_in(text) {
            if !names.contains(&t) {
                names.push(t);
            }
        }
    }
    names
        .into_iter()
        .map(|n| {
            let v = if n.ends_with("_arn") && n.contains("api_key") {
                "arn:aws:secretsmanager:eu-west-2:123456789012:secret:api\"key\\x".to_string()
            } else {
                format!("dummy-{n}")
            };
            (n, v)
        })
        .collect()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ttg-k8s-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn aws_manifests_replace_the_deployment_and_target_group_steps() {
    let cat = Catalog::builtin();
    let p = example();

    // With the manifests off the operator is told to write them by hand.
    let mut off = p.clone();
    off.settings.kubernetes_manifests = false;
    let before = generate(&off, &cat, "aws", Tool::OpenTofu).unwrap();
    let t = titles(&before);
    assert_eq!(
        t.iter()
            .filter(|s| s.contains("Apply the Deployment and its ServiceAccount"))
            .count(),
        3,
        "{t:?}"
    );
    assert!(t.iter().any(|s| s.contains("Bind the target group")), "{t:?}");
    assert!(t.iter().any(|s| s.contains("EFS CSI driver")), "{t:?}");
    assert!(!before.files.keys().any(|f| f.starts_with("k8s/")));
    assert!(!before.files["outputs.tf"].contains("output \"k8s_"));

    // With them on those five steps are gone, and one about the controllers the
    // manifests use takes their place.
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let t = titles(&g);
    assert!(
        !t.iter().any(|s| s.contains("Apply the Deployment")
            || s.contains("Bind the target group")
            || s.contains("EFS CSI driver")),
        "{t:?}"
    );
    assert!(
        t.contains(&"Kubernetes manifests: install KEDA and the AWS Load Balancer Controller".to_string()),
        "{t:?}"
    );
    assert_eq!(before.manual_steps.len(), 11, "{:?}", titles(&before));
    assert_eq!(g.manual_steps.len(), 7, "{t:?}");

    let files: Vec<&String> = g.files.keys().filter(|f| f.starts_with("k8s/")).collect();
    assert_eq!(
        files,
        vec![
            "k8s/00-namespaces.yaml",
            "k8s/api.yaml",
            "k8s/asr-worker.yaml",
            "k8s/reporting.yaml",
            "k8s/render.sh",
            "k8s/render.ps1",
            "k8s/README.md"
        ]
    );
    assert!(g.files["README.md"].contains("## Kubernetes manifests"));

    // The API: behind the load balancer, autoscaled on CPU, image from the registry.
    let api = &g.files["k8s/api.yaml"];
    for want in [
        "kind: ServiceAccount\nmetadata:\n  name: api\n  namespace: platform",
        "image: \"${k8s_images_registry}/api:1.4.2\"",
        "- name: QUEUE_JOBS_URL\n              value: \"${k8s_jobs_url}\"",
        "- name: SECRET_API_KEY_ARN\n              value: \"${k8s_api_key_arn}\"",
        "- name: DB_CORE_DB_HOST\n              value: \"${k8s_core_db_host}\"",
        "kind: Service\n",
        "apiVersion: elbv2.k8s.aws/v1beta1\nkind: TargetGroupBinding",
        "targetGroupARN: \"${k8s_edge_target_group_arn}\"\n  targetType: ip",
        "serviceRef:\n    name: api\n    port: 8080",
        "kind: HorizontalPodAutoscaler",
        "minReplicas: 2\n  maxReplicas: 6",
    ] {
        assert!(api.contains(want), "missing {want:?} in\n{api}");
    }
    // EKS Pod Identity binds the ServiceAccount by name: no annotation.
    assert!(!api.contains("annotations"), "{api}");
    // An autoscaled Deployment leaves its replica count to the autoscaler.
    assert!(!api.contains("replicas: 2\n  selector"), "{api}");

    // The GPU worker: on the GPU pool, one GPU, scaled on the queue by KEDA to zero.
    let asr = &g.files["k8s/asr-worker.yaml"];
    for want in [
        "nodeSelector:\n        workload: asr",
        "- key: nvidia.com/gpu\n          operator: Equal\n          value: present\n          effect: NoSchedule",
        "nvidia.com/gpu: \"1\"",
        "memory: \"8Gi\"",
        "driver: efs.csi.aws.com\n    volumeHandle: \"${k8s_models_id}\"",
        "claimName: asr-worker-models",
        "mountPath: \"/mnt/models\"",
        "kind: ScaledObject",
        "minReplicaCount: 0\n  maxReplicaCount: 4",
        "- type: aws-sqs-queue",
        "queueURL: \"${k8s_jobs_url}\"\n        queueLength: \"2\"\n        awsRegion: \"${k8s_region}\"",
        "kind: TriggerAuthentication",
        "podIdentity:\n    provider: aws",
    ] {
        assert!(asr.contains(want), "missing {want:?} in\n{asr}");
    }
    // A plain worker keeps its replica count.
    assert!(g.files["k8s/reporting.yaml"].contains("  replicas: 1\n"));

    // Every token is an output, and every k8s_ output is a token.
    let out = &g.files["outputs.tf"];
    let mut tokens: Vec<String> = Vec::new();
    for (_, text) in manifests(&g) {
        tokens.extend(k8s::tokens_in(text));
    }
    for t in &tokens {
        assert!(
            out.contains(&format!("output \"{t}\"")),
            "no output for {t}\n{out}"
        );
    }
    for line in out.lines().filter(|l| l.starts_with("output \"k8s_")) {
        let name = line.trim_start_matches("output \"").trim_end_matches("\" {");
        assert!(tokens.iter().any(|t| t == name), "unused output {name}");
    }
    for want in [
        "value       = aws_sqs_queue.jobs.url",
        "value       = aws_lb_target_group.edge_tg.arn",
        "value       = aws_efs_file_system.models.id",
        "value       = var.region",
        "value       = split(\"/\", concat([aws_ecr_repository.images_repo_api.repository_url, aws_ecr_repository.images_repo_asr_worker.repository_url, aws_ecr_repository.images_repo_reporting.repository_url])[0])[0]",
    ] {
        assert!(out.contains(want), "missing {want:?} in\n{out}");
    }
    // The render scripts read exactly those outputs.
    let sh = &g.files["k8s/render.sh"];
    let ps = &g.files["k8s/render.ps1"];
    for t in &tokens {
        assert!(sh.contains(&format!("  {t}\n")), "{sh}");
        assert!(ps.contains(&format!("\"{t}\"")), "{ps}");
    }
    assert!(sh.contains("tf=\"${TF:-tofu}\""), "{sh}");
    let tf = generate(&p, &cat, "aws", Tool::Terraform).unwrap();
    assert!(tf.files["k8s/render.sh"].contains("tf=\"${TF:-terraform}\""));

    // The links the manifests read never reach the Terraform: no depends_on on the
    // registry, node pool or load balancer, and no "link by hand" or "cannot express".
    let c = &g.files["container.tf"];
    assert!(!c.contains("aws_ecr_repository.images_repo_api,"), "{c}");
    assert!(!t.iter().any(|s| s.contains("by hand")), "{t:?}");
    assert!(
        !g.diagnostics.iter().any(|d| d.message.contains("cannot express")
            && (d.message.contains("images") || d.message.contains("gpu") || d.message.contains("edge"))),
        "{:?}",
        g.diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn azure_and_gcp_manifests_carry_their_identity_conventions() {
    let cat = Catalog::builtin();
    let p = example();

    let az = generate(&p, &cat, "azure", Tool::OpenTofu).unwrap();
    let t = titles(&az);
    assert!(!t.iter().any(|s| s.contains("Label the pods")), "{t:?}");
    assert!(!t.iter().any(|s| s.contains("Azure Files CSI")), "{t:?}");
    assert_eq!(
        t.iter()
            .filter(|s| s.contains("Let the Kubernetes cluster pull from the registry"))
            .count(),
        1,
        "one AcrPull step for the registry, not one per workload: {t:?}"
    );
    assert!(
        t.contains(&"Kubernetes manifests: install KEDA".to_string()),
        "{t:?}"
    );
    let asr = &az.files["k8s/asr-worker.yaml"];
    for want in [
        "annotations:\n    azure.workload.identity/client-id: \"${k8s_asr_role_client_id}\"",
        "azure.workload.identity/use: \"true\"",
        "- key: kubernetes.azure.com/scalesetpriority",
        "driver: file.csi.azure.com",
        "shareName: \"${k8s_models_share}\"\n      protocol: nfs",
        "- type: azure-servicebus",
        "queueName: \"${k8s_jobs_name}\"\n        namespace: \"${k8s_jobs_namespace}\"",
        "provider: azure-workload",
        "- name: QUEUE_JOBS_NAMESPACE",
    ] {
        assert!(asr.contains(want), "missing {want:?} in\n{asr}");
    }
    assert!(!az.files["k8s/api.yaml"].contains("TargetGroupBinding"));
    assert!(az.files["k8s/README.md"].contains("An Azure Load Balancer cannot target pods"));
    assert!(
        az.files["outputs.tf"].contains("value       = azurerm_user_assigned_identity.asr_role.client_id")
    );

    let gcp = generate(&p, &cat, "gcp", Tool::OpenTofu).unwrap();
    let t = titles(&gcp);
    assert!(
        !t.iter().any(|s| s.contains("Annotate the ServiceAccount")),
        "{t:?}"
    );
    assert!(!t.iter().any(|s| s.contains("Filestore CSI")), "{t:?}");
    let api = &gcp.files["k8s/api.yaml"];
    for want in [
        "iam.gke.io/gcp-service-account: \"${k8s_api_role_email}\"",
        "cloud.google.com/neg: \"{\\\"exposed_ports\\\":{\\\"8080\\\":{}}}\"",
        "- name: QUEUE_JOBS_TOPIC",
    ] {
        assert!(api.contains(want), "missing {want:?} in\n{api}");
    }
    let asr = &gcp.files["k8s/asr-worker.yaml"];
    for want in [
        "driver: filestore.csi.storage.gke.io\n    volumeHandle: \"${k8s_models_volume_handle}\"",
        "- type: gcp-pubsub",
        "subscriptionName: \"${k8s_jobs_subscription}\"\n        mode: SubscriptionSize",
        "provider: gcp",
    ] {
        assert!(asr.contains(want), "missing {want:?} in\n{asr}");
    }
    assert!(gcp.files["outputs.tf"].contains(
        "value       = format(\"%s-docker.pkg.dev/%s/%s\", google_artifact_registry_repository.images.location, google_artifact_registry_repository.images.project, google_artifact_registry_repository.images.repository_id)"
    ));
}

#[test]
fn manifests_are_deterministic_and_stale_ones_are_removed() {
    let cat = Catalog::builtin();
    let p = example();
    let a = temp_dir("det-a");
    let b = temp_dir("det-b");
    for provider in ["aws", "azure", "gcp"] {
        let ra = ttg_codegen::export(&p, &cat, provider, Tool::OpenTofu, &a.join(provider)).unwrap();
        let rb = ttg_codegen::export(&p, &cat, provider, Tool::OpenTofu, &b.join(provider)).unwrap();
        assert_eq!(ra.files, rb.files);
        for f in &ra.files {
            let x = std::fs::read(a.join(provider).join(f)).unwrap();
            let y = std::fs::read(b.join(provider).join(f)).unwrap();
            assert!(x == y, "{provider}/{f} differs between two exports");
        }
    }
    // Exporting again without the manifests removes what the first export wrote there,
    // but never the render scripts' own output.
    let dir = a.join("aws");
    std::fs::create_dir_all(dir.join("k8s/rendered")).unwrap();
    std::fs::write(dir.join("k8s/rendered/api.yaml"), "kept").unwrap();
    let mut off = p.clone();
    off.settings.kubernetes_manifests = false;
    let g = generate(&off, &cat, "aws", Tool::OpenTofu).unwrap();
    let diffs = ttg_codegen::diff::against_dir(&g, &dir);
    assert!(
        diffs
            .iter()
            .any(|d| d.name == "k8s/api.yaml" && d.status == ttg_codegen::diff::FileStatus::Removed),
        "{:?}",
        diffs.iter().map(|d| (&d.name, d.status)).collect::<Vec<_>>()
    );
    ttg_codegen::export(&off, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    assert!(!dir.join("k8s/api.yaml").exists());
    assert!(!dir.join("k8s/render.sh").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("k8s/rendered/api.yaml")).unwrap(),
        "kept"
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// Parse everything `kubectl annotate --local -o json` prints: one object, a `List`, or
/// several of either.
fn kubectl_objects(out: &str) -> Vec<serde_json::Value> {
    let mut objs = Vec::new();
    for v in serde_json::Deserializer::from_str(out).into_iter::<serde_json::Value>() {
        let v = v.expect("kubectl prints JSON");
        if v["kind"] == "List" {
            objs.extend(v["items"].as_array().cloned().unwrap_or_default());
        } else {
            objs.push(v);
        }
    }
    objs
}

#[test]
fn rendered_manifests_pass_kubectl() {
    let kubectl = Command::new("kubectl").args(["version", "--client"]).output();
    if kubectl.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        if std::env::var("TTG_REQUIRE_KUBECTL").is_ok() {
            panic!("TTG_REQUIRE_KUBECTL is set but kubectl is not installed");
        }
        eprintln!("skipping: kubectl not found");
        return;
    }
    let cat = Catalog::builtin();
    let p = example();
    let root = temp_dir("kubectl");
    for provider in ["aws", "azure", "gcp"] {
        let g = generate(&p, &cat, provider, Tool::OpenTofu).unwrap();
        let values = dummy_values(&g);
        let dir = root.join(provider);
        std::fs::create_dir_all(&dir).unwrap();
        let mut documents = 0;
        let mut expected: BTreeMap<String, usize> = BTreeMap::new();
        for (name, text) in manifests(&g) {
            let rendered = k8s::render_tokens(text, &values);
            assert!(
                !rendered.contains("${"),
                "{provider}/{name} keeps a token:\n{rendered}"
            );
            documents += rendered.matches("\n---\n").count();
            for line in rendered.lines().filter(|l| l.starts_with("kind: ")) {
                *expected.entry(line[6..].to_string()).or_default() += 1;
            }
            std::fs::write(dir.join(name.trim_start_matches("k8s/")), rendered).unwrap();
        }
        let out = Command::new("kubectl")
            .args(["annotate", "--local", "-o", "json", "-f"])
            .arg(&dir)
            .arg("ttg.check/parsed=true")
            .output()
            .expect("kubectl runs");
        assert!(
            out.status.success(),
            "{provider}: kubectl rejected the manifests:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let objs = kubectl_objects(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(objs.len(), documents, "{provider}: every document is one object");
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for o in &objs {
            assert!(o["apiVersion"].as_str().is_some_and(|s| !s.is_empty()), "{o}");
            assert!(
                o["metadata"]["name"].as_str().is_some_and(|s| !s.is_empty()),
                "{o}"
            );
            *kinds.entry(o["kind"].as_str().unwrap().to_string()).or_default() += 1;
        }
        assert_eq!(kinds, expected, "{provider}");
        eprintln!("{provider}: kubectl parsed {} objects: {kinds:?}", objs.len());

        // Values come back exactly, including the quote and backslash the scripts escape.
        let api = objs
            .iter()
            .find(|o| o["kind"] == "Deployment" && o["metadata"]["name"] == "api")
            .expect("the api Deployment");
        let env = api["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        for e in env {
            let value = e["value"].as_str().unwrap();
            assert!(values.values().any(|v| v == value), "{e}");
        }
        if provider == "aws" {
            let secret = env.iter().find(|e| e["name"] == "SECRET_API_KEY_ARN").unwrap();
            assert_eq!(
                secret["value"],
                "arn:aws:secretsmanager:eu-west-2:123456789012:secret:api\"key\\x"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Export for AWS, then run a render script against a stand-in for the Terraform binary
/// and compare what it wrote with what `render_tokens` gives.
fn check_render_script(
    tag: &str,
    run: impl Fn(&Path, &Path, &IndexMap<String, String>) -> Option<std::process::Output>,
) {
    let cat = Catalog::builtin();
    let p = example();
    let dir = temp_dir(tag);
    ttg_codegen::export(&p, &cat, "aws", Tool::OpenTofu, &dir).unwrap();
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let values = dummy_values(&g);
    let Some(out) = run(&dir, &dir.join("k8s"), &values) else {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    assert!(
        out.status.success(),
        "{tag} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for (name, text) in manifests(&g) {
        let got = std::fs::read_to_string(dir.join("k8s/rendered").join(name.trim_start_matches("k8s/")))
            .unwrap_or_else(|e| panic!("{tag}: {name} not rendered: {e}"));
        assert_eq!(got, k8s::render_tokens(text, &values), "{tag}: {name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn bash() -> Option<PathBuf> {
    if let Ok(b) = std::env::var("TTG_BASH") {
        return Some(PathBuf::from(b));
    }
    // On Windows `bash` on PATH is usually WSL's, which cannot run a Windows path.
    let candidates: &[&str] = if cfg!(windows) {
        &["C:/Program Files/Git/bin/bash.exe"]
    } else {
        &["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

#[test]
fn render_sh_fills_in_the_outputs() {
    let Some(bash) = bash() else {
        eprintln!("skipping: no bash found (set TTG_BASH)");
        return;
    };
    check_render_script("render-sh", |root, k8s_dir, values| {
        // `<tf> -chdir=<dir> output -raw <name>` prints the value of <name>.
        let mut fake = String::from("#!/usr/bin/env bash\ncase \"${@: -1}\" in\n");
        for (n, v) in values {
            let quoted = v.replace('\'', "'\\''");
            fake.push_str(&format!("  {n}) printf '%s' '{quoted}' ;;\n"));
        }
        fake.push_str("  *) echo \"no output $*\" >&2; exit 1 ;;\nesac\n");
        let fake_path = root.join("fake-tf.sh");
        std::fs::write(&fake_path, fake).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let slash = |p: &Path| p.to_string_lossy().replace('\\', "/");
        Some(
            Command::new(&bash)
                .arg(slash(&k8s_dir.join("render.sh")))
                .env("TF", slash(&fake_path))
                .output()
                .expect("bash runs"),
        )
    });
}

#[test]
fn render_ps1_fills_in_the_outputs() {
    let shell = if cfg!(windows) { "powershell" } else { "pwsh" };
    if Command::new(shell)
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: {shell} not found");
        return;
    }
    check_render_script("render-ps1", |root, k8s_dir, values| {
        // `<tf> -chdir=<dir> output -json`, the shape Terraform prints.
        let json: serde_json::Map<String, serde_json::Value> = values
            .iter()
            .map(|(n, v)| {
                (
                    n.clone(),
                    serde_json::json!({ "sensitive": false, "type": "string", "value": v }),
                )
            })
            .collect();
        std::fs::write(
            root.join("outputs.json"),
            serde_json::Value::Object(json).to_string(),
        )
        .unwrap();
        let fake_path = if cfg!(windows) {
            let p = root.join("fake-tf.cmd");
            std::fs::write(&p, "@type \"%~dp0outputs.json\"\r\n").unwrap();
            p
        } else {
            let p = root.join("fake-tf.sh");
            std::fs::write(&p, "#!/bin/sh\ncat \"$(dirname \"$0\")/outputs.json\"\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p
        };
        Some(
            Command::new(shell)
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(k8s_dir.join("render.ps1"))
                .arg("-Tool")
                .arg(&fake_path)
                .output()
                .expect("PowerShell runs"),
        )
    });
}

/// What the diagram does not say becomes a manual step, never a guess; an external
/// resource's values become input variables, as any reference to it does.
#[test]
fn missing_images_and_external_resources() {
    let cat = Catalog::builtin();
    let mut p = example();
    // No tag on the api; no registry and no image on the reporting workload; the job
    // queue managed outside this configuration.
    p.nodes.get_mut("wl-api").unwrap().config.remove("image_tag");
    p.edges
        .retain(|e| !(e.source == "wl-report" && e.target == "reg-images"));
    p.nodes.get_mut("q-jobs").unwrap().manual = true;
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    let t = titles(&g);
    assert!(
        t.contains(&"Kubernetes Workload \"api\": set the image tag".to_string()),
        "{t:?}"
    );
    assert!(
        t.contains(&"Kubernetes Workload \"reporting\": name the container image".to_string()),
        "{t:?}"
    );
    assert!(g.files["k8s/api.yaml"].contains(&format!(
        "image: \"${{k8s_images_registry}}/api:{}\"",
        k8s::IMAGE_TAG_PLACEHOLDER
    )));
    assert!(g.files["k8s/reporting.yaml"].contains("image: \"registry.invalid/reporting:0.9.0\""));
    let out = &g.files["outputs.tf"];
    assert!(
        out.contains("output \"k8s_jobs_url\" {\n  description = \"URL of the Event Queue \\\"jobs\\\", read by the Kubernetes manifests\"\n  value       = var.jobs_url\n}"),
        "{out}"
    );
    assert!(g.files["variables.tf"].contains("variable \"jobs_url\""));
    assert!(
        t.iter()
            .any(|s| s.starts_with("Create Event Queue \"jobs\" by hand")),
        "{t:?}"
    );

    // A registry that lists several repositories needs the workload to pick one.
    p.nodes
        .get_mut("wl-asr")
        .unwrap()
        .config
        .insert("repository".into(), ttg_core::Value::Str("whisper".into()));
    let g = generate(&p, &cat, "aws", Tool::OpenTofu).unwrap();
    assert!(
        titles(&g).contains(&"Kubernetes Workload \"asr worker\": choose the image repository".to_string()),
        "{:?}",
        titles(&g)
    );
}

/// The mapping-language additions the export relies on are checked at load time like
/// everything else: `{ setting = … }` conditions, `connection` tables, `env_prefix`, and
/// `manifests = true` on fields and relations.
#[test]
fn manifest_mapping_features_are_validated() {
    let aws = include_str!("../../../definitions/providers/aws.toml");
    let def = |version: u32, extra_resource: &str, extra: &str| {
        format!(
            r#"
schema_version = {version}
[resource]
type = "thing"
category = "network"
display_name = "Thing"
{extra_resource}
[[fields]]
name = "size"
type = "int"
[providers.aws]
status = "partial"
[[providers.aws.blocks]]
key = "main"
resource = "aws_sqs_queue"
[[providers.aws.manual_steps]]
title = "Only without the manifests"
when = {{ setting = "kubernetes_manifests", equals = "false" }}
{extra}
"#
        )
    };
    let load = |src: String| {
        Catalog::from_sources(
            [("thing.toml", src.as_str())].into_iter(),
            [("aws.toml", aws)].into_iter(),
        )
    };
    let reject = |src: String, needle: &str| {
        let err = load(src).expect_err("must be rejected");
        assert!(err.to_string().contains(needle), "{needle}: {err}");
    };
    // The valid forms load.
    load(def(
        2,
        "env_prefix = \"THING\"",
        "[providers.aws.connection]\nURL = { self_block = \"main\", attr = \"url\" }\nSIZE = { field = \"size\" }",
    ))
    .unwrap_or_else(|e| panic!("should load: {e}"));
    reject(def(1, "", ""), "schema_version 2 features");
    reject(
        def(2, "", "").replace("\"kubernetes_manifests\"", "\"nightly\""),
        "unknown setting 'nightly'",
    );
    reject(
        def(
            2,
            "",
            "[providers.aws.connection]\nurl = { self_block = \"main\", attr = \"url\" }",
        ),
        "connection key 'url' must be upper case",
    );
    reject(
        def(2, "", "[providers.aws.connection]\nURL = { field = \"missing\" }"),
        "connection 'URL': undeclared field 'missing'",
    );
    reject(
        def(
            2,
            "",
            "[providers.aws.connection]\nURL = { self_block = \"nope\", attr = \"url\" }",
        ),
        "self_block 'nope' does not exist",
    );
    reject(
        def(2, "env_prefix = \"thing\"", ""),
        "env_prefix 'thing' must be upper case",
    );
    reject(
        def(2, "", "").replace("type = \"int\"", "type = \"int\"\nmanifests = true"),
        "manifests = true is only valid on a type the Kubernetes export reads",
    );
    reject(
        def(2, "", "").replace(
            "[providers.aws]",
            "[[relations]]\nkind = \"attachment\"\ntargets = [\"subnet\"]\nmanifests = true\n[providers.aws]",
        ),
        "relation 'attachment': manifests = true is only valid",
    );
}
