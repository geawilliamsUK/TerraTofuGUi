# Contributing to TerraTofu GUI

Thanks for helping. Most contributions are **data**: a resource definition (one TOML file)
that teaches the app a new abstract type, or a better mapping for an existing one on one
provider. Code contributions are welcome too; the crate layout is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Set-up

- Rust 1.88 or newer (`rust-toolchain` is the workspace `rust-version`).
- [OpenTofu](https://opentofu.org) or Terraform on your `PATH` if you want to run
  `validate` on generated output locally (CI always does).
- Linux needs the usual egui build packages: `libgtk-3-dev libxkbcommon-dev libssl-dev`.

```bash
cargo build                       # everything, including the desktop app
cargo run -p ttg-cli -- catalog   # load + validate every definition
cargo test --workspace            # unit tests, export tests, schema check, MCP headless test
TTG_REQUIRE_VALIDATE=1 cargo test --workspace   # ... plus `tofu validate` on every example x provider x tool
```

Before opening a pull request run `cargo fmt --all`, `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo clippy -p ttg-app --no-default-features` (the
build without the MCP feature must stay warning-free too).

## Adding or changing a resource definition

Definitions live in `definitions/resources/<type_id>.toml`; the format is documented in
[docs/MAPPING_FORMAT.md](docs/MAPPING_FORMAT.md). The short version:

1. **Start from a stub or a neighbour.**
   `cargo run -p ttg-cli -- catalog --example my_type > definitions/resources/my_type.toml`
   prints a skeleton with a section per provider, or copy the closest existing file.
2. **Describe the concept abstractly**: `[resource]`, the `[[fields]]` a user fills in on
   any cloud, and the `[[relations]]` (links) it may have. Keep provider-specific knobs
   out of the abstract fields; they belong in `[[providers.<id>.fields]]`.
3. **Map it per provider.** Every provider in `definitions/providers/` gets a
   `[providers.<id>]` section with a `status`:
   - `full` — the generated HCL does everything the diagram says.
   - `partial` — something is left for the operator; say what in `manual_steps`
     (each may carry `when` so it only appears when it applies). The export shows
     "partial mapping: N manual step(s)" only when a step actually applies.
   - `logical` — nothing to generate (an internet gateway on Azure, a resource group on
     AWS). Say why in `notes`.
   Prefer a design-time `[[providers.<id>.checks]]` over a manual step when the diagram
   itself can be checked (a subnet that must be delegated, a size that is too small).
   A relation that only some providers can use gets `providers = [...]` on the relation,
   so the others stay quiet instead of warning "cannot express".
4. **Verify the argument names against the real schema**:
   `cargo run -p ttg-cli -- schema show aws aws_thing` lists every argument and nested
   block with its type and whether it is required. The `schema_check` test fails the
   build when a mapping names a resource type or argument the provider does not have.
5. **Register it**: add the file to `BUILTIN_RESOURCES` in
   `crates/ttg-catalog/src/load.rs` (one line) so it is embedded in the binaries. Until
   then you can test with `--definitions ./definitions`.
6. **Prove it with an example**: add the type to an existing project under `examples/`
   (or add a new one) and run the export for every provider:
   ```bash
   cargo run -p ttg-cli -- export examples/my-example.ttg.json --provider aws --out out/aws --validate
   ```
   The `validate_examples` test does this for every example, provider and tool; it must
   stay green. Keep example names stable: the export tests assert on them.
7. `cargo run -p ttg-cli -- catalog --strict` reports abstract fields no provider uses,
   outputs that name attributes the provider does not have, and relations no mapping
   consumes. Fix or justify each finding in the definition's comments.

### Worked example: a Security Group

`definitions/resources/security_group.toml` shows most of the language in one file:

- an abstract `struct_list` field (`rules`) with typed items, one of which is an
  `entity_ref` to another security group;
- AWS: one standalone rule resource per row through `for_each_field`, with `when` on
  the item's direction and `if` / `else` port handling for `protocol = "all"`;
- Azure: the same rows as `azurerm_network_security_rule` resources with priorities from
  `item_index`, and a group-to-group source rendered as the `VirtualNetwork` service tag
  because Azure has no such rule;
- GCP: firewall rules per row, and membership expressed as a network tag kept in a
  `terraform_data` resource so instances and other rules can reference it;
- design-time checks (a rule without a CIDR or source group is an error; a rule open to
  the internet on an unusual port is a warning).

### Refreshing the provider schema index

The bundled index (`crates/ttg-schema/data/index.json.gz`) is generated from the
installed tool: `cargo run -p ttg-cli -- schema refresh --out
crates/ttg-schema/data/index.json.gz`. Refresh it when the provider version constraints
in `definitions/providers/*.toml` change; the `schema_check` test then tells you which
mappings the new provider versions broke.

## Examples

Example projects under `examples/*.ttg.json` are documentation and test fixtures at once.
Every one of them must export and validate for every provider and both tools. Give
globally unique names (buckets, function apps, database servers) a `-x7q2`-style suffix
so two people applying the same example do not collide.

## Code changes

- Keep the crate boundaries: `ttg-core` knows nothing about providers; `ttg-catalog`
  loads and validates definitions; `ttg-codegen` turns a project plus catalog into HCL
  and diagnostics; `ttg-app` is the desktop app; `ttg-cli` is the headless front end.
- New mapping-language features need: the `schema.rs` struct, a `validate.rs` rule that
  rejects misuse at load time, the `emit.rs` / `diagnostics.rs` behaviour, a line in
  `docs/MAPPING_FORMAT.md`, and a test in `crates/ttg-codegen/tests/export.rs`.
- The project file format is described by `schemas/project.schema.json`, generated from
  the IR types. After changing `ttg_core::ir`, regenerate it with
  `cargo run -p ttg-cli -- schema project --out schemas/project.schema.json`; a test
  fails when the checked-in copy is stale. Additions to the format must keep older files
  loadable (`#[serde(default)]`).

## Pull requests and commits

- One topic per pull request; describe what a user can now do, not the diff.
- Plain commit messages in the imperative ("Add Network Peering type"); no bot
  trailers.
- CI (`.github/workflows/ci.yml`) runs formatting, clippy for both feature sets, the
  full test suite with `tofu validate`, the strict catalog check and an export of the
  examples. Green CI is required to merge.
