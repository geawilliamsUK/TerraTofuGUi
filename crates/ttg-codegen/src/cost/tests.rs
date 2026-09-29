//! Coverage and price-list consistency: no mapped type is silently left out, every row a
//! model asks for exists, and the lists themselves hold together.

use super::*;
use ttg_core::{Container, Node, Position, Size};

/// A project with one entity of every catalog type, all fields at their defaults.
fn one_of_everything(cat: &Catalog) -> Project {
    let mut p = Project::new("everything");
    for (i, (type_id, def)) in cat.resources.iter().enumerate() {
        let id = format!("e{i}");
        if def.resource.kind == ttg_catalog::ResourceKind::Container {
            p.containers.insert(
                id.clone(),
                Container {
                    id,
                    name: type_id.clone(),
                    container_type: type_id.clone(),
                    config: Default::default(),
                    provider_config: Default::default(),
                    position: Position {
                        x: i as i32 * 500,
                        y: 0,
                    },
                    size: Size::default(),
                    parent: None,
                    manual: false,
                    providers: Vec::new(),
                    extra: Default::default(),
                },
            );
        } else {
            p.nodes.insert(
                id.clone(),
                Node {
                    id,
                    name: type_id.clone(),
                    resource_type: type_id.clone(),
                    config: Default::default(),
                    provider_config: Default::default(),
                    position: Position {
                        x: i as i32 * 200,
                        y: 1000,
                    },
                    size: None,
                    parent: None,
                    manual: false,
                    providers: Vec::new(),
                    extra: Default::default(),
                },
            );
        }
    }
    p
}

#[test]
fn no_mapped_type_is_silently_left_out() {
    let cat = Catalog::builtin();
    let mut missing = Vec::new();
    for provider in PriceBook::providers() {
        for type_id in cat.resources.keys() {
            if coverage(&cat, provider, type_id) == Coverage::Missing {
                missing.push(format!("{provider}: {type_id}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these mapped types have no cost model and are neither [free] nor [not_estimated] in definitions/prices/<provider>.toml:\n{}",
        missing.join("\n")
    );
}

#[test]
fn free_and_not_estimated_lists_name_real_types_and_no_priced_ones() {
    let cat = Catalog::builtin();
    for provider in PriceBook::providers() {
        let book = PriceBook::for_provider(provider).unwrap();
        for type_id in book.free.keys().chain(book.not_estimated.keys()) {
            assert!(
                cat.resource(type_id).is_some(),
                "{provider}.toml lists unknown type {type_id}"
            );
            assert!(
                matches!(model_for(provider, type_id), Model::Missing),
                "{provider}: {type_id} is listed as free / not estimated but also has a priced model"
            );
            assert!(
                cat.mapping(type_id, provider).is_some(),
                "{provider}: {type_id} is listed but has no {provider} mapping (it is reported as unmapped anyway)"
            );
        }
        for type_id in book.free.keys() {
            assert!(
                !book.not_estimated.contains_key(type_id),
                "{provider}: {type_id} is both free and not estimated"
            );
        }
    }
}

#[test]
fn every_row_a_model_asks_for_is_in_the_price_list() {
    let cat = Catalog::builtin();
    let p = one_of_everything(&cat);
    for provider in PriceBook::providers() {
        let book = PriceBook::for_provider(provider).unwrap();
        for region in &book.regions {
            let est = estimate(&p, &cat, provider, Some(region)).unwrap();
            for l in &est.lines {
                let bad: Vec<&String> = l
                    .notes
                    .iter()
                    .filter(|n| {
                        n.contains("no bundled price")
                            || n.contains("not included (no bundled price")
                            || n.contains("resolved from the mapping")
                    })
                    .collect();
                assert!(bad.is_empty(), "{provider} {region} {}: {bad:?}", l.resource_type);
                if coverage(&cat, provider, &l.resource_type) == Coverage::Priced {
                    assert_eq!(
                        l.status,
                        Status::Priced,
                        "{provider} {region} {}: {:?}",
                        l.resource_type,
                        l.notes
                    );
                }
                for u in &l.assumptions {
                    assert!(
                        assumption(&u.key).is_some(),
                        "{provider} {}: unknown assumption {}",
                        l.resource_type,
                        u.key
                    );
                }
            }
        }
    }
}

#[test]
fn price_lists_are_consistent() {
    for provider in PriceBook::providers() {
        let book = PriceBook::for_provider(provider).unwrap();
        assert_eq!(book.provider, provider);
        assert_eq!(book.currency, "USD");
        assert!(
            book.retrieved.len() == 10 && book.retrieved.as_bytes()[4] == b'-',
            "{provider}: retrieved must be YYYY-MM-DD"
        );
        for (name, t) in &book.tables {
            assert!(!t.source.is_empty(), "{provider}.{name}: no source");
            assert!(
                !t.checked.is_empty(),
                "{provider}.{name}: say what was checked, even if nothing"
            );
            for (sku, row) in &t.rows {
                assert!(!row.prices.is_empty(), "{provider}.{name}.{sku}: no prices");
                for (region, v) in &row.prices {
                    assert!(
                        region == "*" || book.regions.contains(region),
                        "{provider}.{name}.{sku}: unknown region {region}"
                    );
                    assert!(
                        v.is_finite() && *v >= 0.0,
                        "{provider}.{name}.{sku}.{region}: {v}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_price_list_with_an_unknown_key_is_refused() {
    let text = "schema_version = 1\nprovider = \"x\"\ncurrency = \"USD\"\nretrieved = \"2026-01-01\"\nregions = [\"r\"]\nfallback_region = \"r\"\nbasis = \"b\"\n[tables.t]\nunit = \"hour\"\nsorce = \"typo\"\n";
    assert!(PriceBook::parse(text).is_err());
    let euros = text.replace("USD", "EUR").replace("sorce", "source");
    assert!(PriceBook::parse(&euros).unwrap_err().contains("USD"));
}
