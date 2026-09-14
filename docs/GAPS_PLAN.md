# Closing the gaps from the CallScope design

Execution plan for the findings in the CallScope gap report (an agent built a 111-node
production design through the MCP and listed what it could not express). Acceptance is
that report's own yardstick: the AWS export of that design needs no `$raw` and a handful
of native resources instead of forty-six, and the Azure and GCP exports carry the same
security posture.

Work is split into packages that touch disjoint parts of the tree so they can be built
in parallel and merged; each package lands as one commit.

## Wave A

| Package | Scope | Report items |
|---|---|---|
| **WP1 MCP friction** | natives through `entity_add` / `project_apply`; names in `entity_ref` fields; reads no longer wait behind an approval prompt (writes queue in order); AWS `sg-` name check; `schema_show` `depth` / `required_only`; annotation removal never asks; `$ref` with `block` documented; `--serve` in the server instructions | 1.1–1.5, 6.10, §4 |
| **WP3 Security posture** | `encryption_key` type + `encrypted_with` relation; object storage hardening (public access block, TLS-only policy, lifecycle, CORS, access logging); database HA / encryption / backups / deletion protection / final snapshot / version / instance class override; queue retention, long polling, encryption; secret recovery window, generated values; log group and topic encryption with the key policy that allows the services; registry immutability, scan on push, lifecycle; project-wide default tags | 2.2, §3 |
| **WP6 Views as documents** | note annotations; logical (annotation-only) nodes flows can attach to; `view_get` and a `view` argument on the drawing tools; `view_fit` and screenshot options; containers follow their members in a view; flow step numbers, colours and label placement; view description and legend; Markdown / Mermaid export of a view; filters by provider layer, native-vs-curated, type id, name glob, hide structural links; nested groups | 6.1–6.9, 6.11–6.13 |

## Wave B

| Package | Scope | Report items |
|---|---|---|
| **WP2 Kubernetes story** | `kubernetes_node_pool` type (free-text instance type, min 0, taints, labels, GPU image, spot); `kubernetes` trusted service on roles; `kubernetes_workload` type whose links generate pod identity (AWS), workload identity (Azure, GCP) and least-privilege policy; container apps get policies from links too; load balancers forward to clusters; clusters log to a log group; cluster version, endpoint access, log types, add-ons | 2.1, 2.5, 2.6, 2.14, 2.15, 6.2 (workload half) |
| **WP4 Internet edge** | `tls_certificate` type and an HTTPS listener; `web_application_firewall` type; `cdn` type; `user_identity` (Cognito / Identity Platform); DNS alias records to a load balancer or CDN | 2.4, 2.7, 2.8, 2.18 |
| **WP5 Operational rest** | dead-letter relation with max receives; `file_system` type with mount targets; `budget` type; AWS interface endpoints without a target, per zone, gateway for S3; repositories on a registry; alarm presets and more watch targets; topic subscriptions; flow logs; `audit_trail` type | 2.3, 2.9–2.13, 2.16, 2.17 |

## Then

Integrate: full suite with `tofu validate` on all three providers, clippy for both
feature sets, the strict catalog check, and an export of the CallScope design as the
acceptance test. Refresh docs (README catalog paragraph, MAPPING_FORMAT, MCP_PLAN) and
the memory of what was learnt.

Deferred, with the reason: a `tier` on subnets so route tables attach themselves
(changes the modelling of routing that the reachability engine depends on; revisit with
the auto-layout work); SVG export of a view (egui has no vector back end; PNG via the
fitted screenshot and Mermaid cover the document use).
