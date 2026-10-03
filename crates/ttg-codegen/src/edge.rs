//! Host names at the edge: the name a DNS Record answers for, the name a CDN serves, the
//! names a TLS Certificate covers, and the mapping language's `hop` that walks from one
//! entity to the entities linked to it (a CDN's origin load balancer, then the records that
//! alias it). The built-in checks here look at those names across several entities: a
//! record that points at a CDN under a name the CDN does not serve, and a CDN that
//! forwards the viewer's Host header to an origin whose certificate does not cover it.

use crate::diagnostics::{
    field_or_default, relation_sources_of_type, relation_targets_of_type, Code, Diagnostic, Severity,
};
use ttg_catalog::{Catalog, Hop};
use ttg_core::{EntityRef, Id, Project, Relation, Value};

/// The entities a hop reaches from each of `first`, in order and without repeats.
pub fn hop(p: &Project, cat: &Catalog, first: &[Id], h: &Hop) -> Vec<Id> {
    let Some(kind) = Relation::from_key(&h.relation) else {
        return Vec::new();
    };
    let mut out: Vec<Id> = Vec::new();
    for f in first {
        let Some(fe) = p.entity(f) else { continue };
        let reached = if h.incoming {
            relation_sources_of_type(p, &fe, kind, h.target_type.as_deref())
        } else {
            relation_targets_of_type(p, cat, &fe, kind, h.target_type.as_deref())
        };
        let names = if h.certificate_covers {
            certificates_of(p, &fe)
                .iter()
                .flat_map(|c| certificate_names(cat, p, c))
                .collect()
        } else {
            Vec::new()
        };
        for r in reached {
            if h.certificate_covers {
                let covered = host_name(p, cat, &r).is_some_and(|n| names.iter().any(|c| covers(c, &n)));
                if !covered {
                    continue;
                }
            }
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// TLS Certificates an entity links to (a load balancer's 'Certificate').
pub fn certificates_of(p: &Project, e: &EntityRef) -> Vec<Id> {
    p.edges_from(e.id)
        .filter(|x| {
            p.entity(&x.target)
                .is_some_and(|t| t.resource_type == "tls_certificate")
        })
        .map(|x| x.target.clone())
        .collect()
}

/// Domain and alternative names of a TLS Certificate, lower case.
pub fn certificate_names(cat: &Catalog, p: &Project, id: &str) -> Vec<String> {
    let Some(c) = p.entity(id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Any provider will do: both fields are abstract.
    let provider = "aws";
    if let Some(Value::Str(d)) = field_or_default(cat, provider, &c, "domain", false) {
        out.push(normal(&d));
    }
    if let Some(Value::List(l)) = c.field("alternative_names") {
        out.extend(l.iter().map(|s| normal(s)));
    }
    out.retain(|s| !s.is_empty());
    out
}

/// The name an entity answers for: a DNS Record's fully qualified name (its record name
/// in its zone's domain, `@` for the apex), a CDN's custom domain.
pub fn host_name(p: &Project, cat: &Catalog, id: &str) -> Option<String> {
    let e = p.entity(id)?;
    match e.resource_type {
        "dns_record" => {
            let zone = relation_targets_of_type(p, cat, &e, Relation::AttributeReference, Some("dns_zone"))
                .into_iter()
                .next()?;
            let ze = p.entity(&zone)?;
            let domain = normal(&field_or_default(cat, "aws", &ze, "domain", false)?.display());
            let name = field_or_default(cat, "aws", &e, "record_name", false)
                .map(|v| normal(&v.display()))
                .unwrap_or_default();
            Some(if name.is_empty() || name == "@" {
                domain
            } else {
                format!("{name}.{domain}")
            })
        }
        "cdn" => e
            .field("domain")
            .map(|v| normal(&v.display()))
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn normal(s: &str) -> String {
    s.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Does a certificate name cover a host name? `*.example.com` covers exactly one label
/// in front of `example.com`, as TLS wildcards do.
pub fn covers(cert_name: &str, host: &str) -> bool {
    let (c, h) = (normal(cert_name), normal(host));
    match c.strip_prefix("*.") {
        Some(rest) => h
            .strip_suffix(rest)
            .and_then(|label| label.strip_suffix('.'))
            .is_some_and(|label| !label.is_empty() && !label.contains('.')),
        None => c == h,
    }
}

/// Built-in checks across the edge entities (see the module comment).
pub fn checks(p: &Project, cat: &Catalog, provider: &str, out: &mut Vec<Diagnostic>) {
    let mut push = |entity: &str, sev: Severity, msg: String| {
        out.push(Diagnostic {
            entity: Some(entity.to_string()),
            severity: sev,
            code: Code::Network,
            message: msg,
            provider: None,
        })
    };
    for e in p.entities() {
        match e.resource_type {
            // A CDN only answers for the custom domain it was given; any other name that
            // DNS sends to it gets an error from the edge.
            "dns_record" => {
                let cdns = relation_targets_of_type(p, cat, &e, Relation::AttributeReference, Some("cdn"));
                let Some(cdn) = cdns.first().and_then(|c| p.entity(c)) else {
                    continue;
                };
                let Some(name) = host_name(p, cat, e.id) else {
                    continue;
                };
                match host_name(p, cat, cdn.id) {
                    None => push(
                        e.id,
                        Severity::Warning,
                        format!(
                            "\"{}\" points {name} at the CDN \"{}\", which has no custom domain and only answers for its own host name; set the CDN's Custom domain to {name}",
                            e.name, cdn.name
                        ),
                    ),
                    Some(d) if d != name => push(
                        e.id,
                        Severity::Warning,
                        format!(
                            "\"{}\" points {name} at the CDN \"{}\", which only answers for its custom domain {d}",
                            e.name, cdn.name
                        ),
                    ),
                    _ => {}
                }
            }
            // Forwarding the viewer's Host header makes CloudFront ask the origin for the
            // viewer's name, so the origin's certificate has to cover it.
            "cdn" if provider == "aws" => {
                let forwards = field_or_default(cat, provider, &e, "forward_host_header", false)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !forwards {
                    continue;
                }
                let origins =
                    relation_targets_of_type(p, cat, &e, Relation::AttributeReference, Some("load_balancer"));
                let Some(lb) = origins.first().and_then(|o| p.entity(o)) else {
                    continue;
                };
                let https = field_or_default(cat, provider, &lb, "protocol", false)
                    .is_some_and(|v| v.display() == "https");
                if !https {
                    continue;
                }
                let names: Vec<String> = certificates_of(p, &lb)
                    .iter()
                    .flat_map(|c| certificate_names(cat, p, c))
                    .collect();
                match host_name(p, cat, e.id) {
                    Some(d) if names.iter().any(|n| covers(n, &d)) => {}
                    Some(d) => push(
                        e.id,
                        Severity::Error,
                        format!(
                            "\"{}\" forwards the viewer's Host header ({d}) to \"{}\", whose certificate does not cover it, so every request would fail TLS at the origin; add {d} to the load balancer's certificate or turn 'Forward Host header' off",
                            e.name, lb.name
                        ),
                    ),
                    None => push(
                        e.id,
                        Severity::Error,
                        format!(
                            "\"{}\" forwards the viewer's Host header but has no custom domain, so the origin \"{}\" would be asked for the CloudFront host name, which its certificate cannot cover; set a custom domain or turn 'Forward Host header' off",
                            e.name, lb.name
                        ),
                    ),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::covers;

    #[test]
    fn wildcards_cover_one_label() {
        assert!(covers("origin.example.com", "ORIGIN.example.com."));
        assert!(covers("*.example.com", "origin.example.com"));
        assert!(!covers("*.example.com", "example.com"));
        assert!(!covers("*.example.com", "a.b.example.com"));
        assert!(!covers("example.com", "www.example.com"));
    }
}
