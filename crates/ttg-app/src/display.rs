//! Abstract vs concrete display of the diagram.
//!
//! In *concrete* mode every node is labelled with the provider resource it generates
//! for the current target provider (`aws_sqs_queue`, `azurerm_servicebus_queue`, ...),
//! helper resources are counted, and the inspector only shows that provider's fields.
//! An optional icon pack (`<definitions>/icons/<provider>/<type>.png`) replaces the
//! glyph square; nothing is shipped in the repository because the official provider
//! icon sets have their own licences.

use crate::app::TtgApp;
use std::collections::HashMap;
use std::path::PathBuf;
use ttg_catalog::MappingStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    #[default]
    Abstract,
    /// Show the diagram in the target provider's terms.
    Concrete,
}

/// What a resource type becomes on a provider.
#[derive(Debug, Clone)]
pub struct Concrete {
    /// Primary resource type (`aws_subnet`), or `None` for a logical mapping.
    pub primary: Option<String>,
    /// Number of additional resource blocks the mapping may emit.
    pub helpers: usize,
    pub status: MappingStatus,
}

impl Concrete {
    /// One-line subtitle for the canvas.
    pub fn subtitle(&self) -> String {
        match (&self.primary, self.helpers) {
            (None, _) => "not emitted".into(),
            (Some(p), 0) => p.clone(),
            (Some(p), n) => format!("{p} +{n}"),
        }
    }
}

/// Lazily loaded icon textures keyed by `provider/type_id`.
#[derive(Default)]
pub struct Icons {
    dirs: Vec<PathBuf>,
    cache: HashMap<String, Option<egui::TextureHandle>>,
}

impl Icons {
    pub fn new(defs: Option<&PathBuf>) -> Self {
        let mut dirs = Vec::new();
        if let Some(d) = defs {
            dirs.push(d.join("icons"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join("definitions").join("icons"));
        }
        if let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        {
            dirs.push(exe.join("definitions").join("icons"));
        }
        Icons {
            dirs,
            cache: HashMap::new(),
        }
    }

    /// The icon for a type on a provider, if an icon pack provides one.
    pub fn get(&mut self, ctx: &egui::Context, provider: &str, type_id: &str) -> Option<egui::TextureHandle> {
        let key = format!("{provider}/{type_id}");
        if let Some(t) = self.cache.get(&key) {
            return t.clone();
        }
        let tex = self
            .dirs
            .iter()
            .map(|d| d.join(provider).join(format!("{type_id}.png")))
            .find(|p| p.is_file())
            .and_then(|p| image::open(&p).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
                ctx.load_texture(key.clone(), ci, egui::TextureOptions::LINEAR)
            });
        self.cache.insert(key, tex.clone());
        tex
    }
}

impl TtgApp {
    pub fn concrete_mode(&self) -> bool {
        self.display == DisplayMode::Concrete
    }

    /// Is the entity part of the layer being displayed? Always true in abstract mode.
    pub fn on_layer(&self, id: &str) -> bool {
        self.layer.as_ref().is_none_or(|l| l.contains(id))
    }

    /// Short tag for entities that are not on every provider: "Azure only", "AWS only".
    pub fn layer_tag(&self, id: &str) -> Option<String> {
        let e = self.project.entity(id)?;
        let tags: &[String] = if let Some(n) = self.project.nodes.get(id) {
            &n.providers
        } else {
            &self.project.containers[id].providers
        };
        let scope = self
            .catalog
            .resource(e.resource_type)
            .map(|d| d.resource.providers.clone())
            .unwrap_or_default();
        let all: Vec<String> = self.catalog.provider_ids();
        let eff: Vec<String> = all
            .iter()
            .filter(|p| (tags.is_empty() || tags.contains(p)) && (scope.is_empty() || scope.contains(p)))
            .cloned()
            .collect();
        if eff.len() == all.len() {
            return None;
        }
        let names: Vec<String> = eff
            .iter()
            .map(|p| {
                self.catalog
                    .provider(p)
                    .map(|d| d.provider.short_name())
                    .unwrap_or(p.clone())
            })
            .collect();
        Some(if names.is_empty() {
            "no provider".into()
        } else {
            format!("{} only", names.join(" / "))
        })
    }

    /// Concrete rendering of an abstract type for the current target provider.
    pub fn concrete(&self, type_id: &str) -> Option<Concrete> {
        let m = self
            .catalog
            .mapping(type_id, &self.project.settings.target_provider)?;
        let primary = m
            .blocks
            .iter()
            .find(|b| b.key == "main")
            .or(m.blocks.first())
            .map(|b| b.resource.clone());
        Some(Concrete {
            helpers: m.blocks.len().saturating_sub(usize::from(primary.is_some())),
            primary,
            status: m.status,
        })
    }

    /// Subtitle for a node: abstract display name, or the concrete resource type.
    pub fn type_subtitle(&self, type_id: &str) -> String {
        let display = self
            .catalog
            .resource(type_id)
            .map(|d| d.resource.display_name.clone())
            .unwrap_or(type_id.to_string());
        if !self.concrete_mode() {
            return display;
        }
        match self.concrete(type_id) {
            Some(c) => c.subtitle(),
            None => match self.provider_scope(type_id) {
                Some(only) => format!("{only} only"),
                None => format!("no {} mapping", self.provider_display_name()),
            },
        }
    }

    /// For a provider-scoped type, the display names of the providers it exists on.
    pub fn provider_scope(&self, type_id: &str) -> Option<String> {
        let def = self.catalog.resource(type_id)?;
        if def.resource.providers.is_empty() {
            return None;
        }
        Some(
            def.resource
                .providers
                .iter()
                .map(|p| {
                    self.catalog
                        .provider(p)
                        .map(|d| d.provider.short_name())
                        .unwrap_or(p.clone())
                })
                .collect::<Vec<_>>()
                .join(" / "),
        )
    }

    pub fn provider_display_name(&self) -> String {
        let p = &self.project.settings.target_provider;
        self.catalog
            .provider(p)
            .map(|d| d.provider.display_name.clone())
            .unwrap_or(p.clone())
    }

    /// Tooltip listing every HCL address an entity would generate on the target provider.
    pub fn concrete_tooltip(&self, id: &str) -> String {
        let addrs = ttg_codegen::emit::addresses_for(
            &self.project,
            &self.catalog,
            &self.project.settings.target_provider,
            id,
        );
        if addrs.is_empty() {
            format!("Nothing generated for {}", self.provider_display_name())
        } else {
            format!(
                "Generates on {}:\n{}",
                self.provider_display_name(),
                addrs.join("\n")
            )
        }
    }
}
