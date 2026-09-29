# Prices for the cost estimate

The cost estimate (`ttg_codegen::cost`, the Cost window, `ttg cost`, the MCP
`cost_estimate` tool) multiplies usage by unit prices bundled with the app in
`definitions/prices/<provider>.toml`. Nothing is fetched at run time: the app works
offline and gives the same answer tomorrow as today. This page says what the figures
are, where they come from, what was checked, and how to refresh them.

## What the figures are — and are not

- **List prices in US dollars**, dated by the file's `retrieved` field, which every
  surface shows next to the total.
- **On-demand / pay-as-you-go, Linux, shared tenancy.** No free tier, no Savings Plans,
  reserved instances, committed- or sustained-use discounts, no enterprise agreements,
  no support plans, no tax. Where a free tier or included allowance would matter the
  line says it was not deducted.
- **For a few regions**: AWS us-east-1, eu-west-1, eu-west-2; Azure eastus, westeurope,
  uksouth; Google Cloud us-central1, europe-west1, europe-west2. A project in another
  region is priced at the list's `fallback_region` (the first of each list), and the
  estimate says so.
- **Usage is assumed**, not known: GB stored, requests, log volume, node-hours a day of a
  pool that scales to zero, and so on (`cost::ASSUMPTIONS`, editable per project and per
  resource). Change them before trusting a total.
- Some things are **left out on purpose** and say why: data transfer to the internet,
  backups beyond the free allowance, throughput modes, anything priced per API call the
  diagram cannot know. Types that cost nothing, or that the estimate does not price, are
  listed in each file's `[free]` / `[not_estimated]` tables with the reason the user sees.

It is an estimate for a design review, not a quote. Use the provider's own calculator
before committing to a budget.

## File layout

```toml
schema_version = 1
provider = "aws"
currency = "USD"                # the only currency the estimate knows
retrieved = "2026-09-29"        # shown with every estimate
regions = ["us-east-1", "eu-west-1", "eu-west-2"]
fallback_region = "us-east-1"   # prices used for a region with no column
basis = "AWS on-demand list prices (Linux, shared tenancy), USD, ..."

[free]                          # abstract type = reason (shown to the user)
security_group = "Security groups cost nothing."

[not_estimated]                 # abstract type = reason
# ...

[tables.nat_gateway]            # one table per service and unit
unit = "hour"
description = "NAT gateway, per hour and per GB processed"
source = "Amazon VPC pricing (aws.amazon.com/vpc/pricing)"
checked = "2026-09-29: ... what was cross-checked, how"

[tables.nat_gateway.rows]       # one row per SKU
hour = { us-east-1 = 0.045, eu-west-1 = 0.048, eu-west-2 = 0.050 }
data = { unit = "GB", "*" = 0.045 }   # "*" = same everywhere; a row may set its own unit
```

The Rust models (`crates/ttg-codegen/src/cost/{aws,azure,gcp}.rs`) look rows up by table
name and SKU. Row keys are the provider's own SKU names wherever the mapping produces
one (`db.t4g.medium`, `Standard_D2s_v3`, `e2-standard-4`), so a model prices whatever the
mapping would create — including a user's instance-type override — as long as the row
exists. A model that asks for a row the list lacks leaves that charge out and says so on
the line ("no bundled price for …"); the test
`every_row_a_model_asks_for_is_in_the_price_list` fails if the *default* SKU of any
type is missing in any bundled region.

## Sources and what was checked

Each table's `source` names the public page the figures come from and its `checked` field
records what was cross-checked against the provider's own machine-readable list, when,
and how (or that nothing could be). In summary, for the 2026-09-29 list:

- **AWS** — every row and region. EC2, EBS, NAT gateway, public IPv4, VPC endpoints
  and RDS against the JSON behind the aws.amazon.com pricing pages
  (`b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/...`); everything else against the
  official Price List Bulk API region offers (`pricing.us-east-1.amazonaws.com`; Route 53
  and CloudFront live in the `aws-other` offer). g6 is not sold in eu-west-1, so that
  column is absent and falls back.
- **Azure** — every row and region against the retail prices API
  (`prices.azure.com/api/retail/prices`, Consumption meters, Linux, no Spot / Low
  Priority). NAT gateway, Load Balancer and Private Link are published under region
  `Global`, DNS and CDN under `Zone 1`. The AKS Free tier has no meter (it is free).
- **Google Cloud** — every row and region against the price tables embedded in the
  cloud.google.com pricing pages (an `AF_initDataCallback` JSON blob per page), or the
  page text for prices that are the same everywhere. No API key is needed. V100 is not
  sold in europe-west1 nor P4 in europe-west2. Cloud Monitoring does not charge for
  alerting yet (announced for no sooner than September 2027), so that row is 0.

Things the list knows about but the models leave out, each named in its table's
`checked` note: EKS extended-support hours, gp3 IOPS / throughput above the baseline,
Azure Standard HDD transactions, load-balancer rules past the first five, Container Apps
idle usage, Cloud NAT IP addresses. RDS Multi-AZ lists at twice single-AZ or a little
more; the model uses twice.

## Refreshing the list

1. Run the checker from the repository root:

   ```sh
   py -3 scripts/prices_check.py              # every provider
   py -3 scripts/prices_check.py --provider aws --tolerance 1
   ```

   It re-fetches every row from the sources above (standard library only, no keys) and
   compares bundled with live per row and region, exiting non-zero when anything moved by
   more than the tolerance (2% by default) or a SKU can no longer be found; `-v` prints
   every comparison, not just the differences. The only skip is a row with no meter (the
   AKS Free tier). On 2026-09-29 every figure agreed.
2. Update the rows it reports, and the `checked` note of each table you touched (what you
   compared, against which endpoint, on which date). For figures the script cannot
   fetch, compare by hand against the page named in `source`.
3. Bump `retrieved` in each file you refreshed.
4. `cargo test -p ttg-codegen cost` — the consistency tests check every row a model uses
   is present, every region column is one of `regions`, and every table has a source and
   a checked note.

## Adding a SKU, a region or a type

- **A SKU** (a new instance type users pick): add a row to the right table, in every
  bundled region it is sold in. The models need no change.
- **A region**: add it to `regions` and a column to every row (a row without the column
  falls back to `fallback_region` and the line says so).
- **A curated type**: either price it in the provider's model file (one function that
  reads the entity's fields, resolves the SKU with `Ctx::sku(block, arg)` from the mapping,
  and calls `Ctx::charge(item, table, row, quantity)`), or list it under `[free]` /
  `[not_estimated]` with the reason. `no_mapped_type_is_silently_left_out` fails until one
  of the three is done for every provider that maps the type.
- **A usage assumption**: add it to `cost::ASSUMPTIONS` with a default, unit and label; the
  Cost window lists it automatically and the MCP tool accepts it.
