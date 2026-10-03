#!/usr/bin/env python3
"""Compare the bundled price lists (definitions/prices/*.toml) with the providers' public prices.

Run from the repository root before bumping a list's `retrieved` date:

    py -3 scripts/prices_check.py                     # every provider
    py -3 scripts/prices_check.py --provider aws      # one provider
    py -3 scripts/prices_check.py --tolerance 0.5     # percent; default 2
    py -3 scripts/prices_check.py --verbose           # also print the rows that match

For every table/row/region it prints the bundled figure, the live figure and the difference,
and exits 1 when any live figure differs by more than the tolerance (or cannot be fetched or
matched any more, which usually means the provider renamed a SKU). Rows that have no public
machine-readable source are listed as SKIP with the reason and never fail the run.

Sources (all public, no credentials, Python standard library only):

* AWS   - the JSON behind the AWS pricing pages (b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps)
          for EC2, EBS, RDS, NAT gateway and VPC, and the Price List Bulk API
          (pricing.us-east-1.amazonaws.com/offers/v1.0/aws) region offer files for the rest.
* Azure - the retail prices API (prices.azure.com/api/retail/prices), Consumption meters.
* GCP   - the data embedded in the cloud.google.com pricing pages: region-selectable tables
          are server-rendered for every region inside an `AF_initDataCallback` blob, and
          prices that are the same everywhere are read from the page text. The Cloud Billing
          Catalog API would be the proper source but needs an API key.

Maintenance: each provider has one mapping, `AWS`, `AZURE` and `GCP` below, of
`table -> row -> source`. A source is built by one of the small helpers (`ec2(...)`,
`bulk(...)`, `az(...)`, `gtable(...)`, `gtext(...)`, ...) and turns a region into a price in
the unit the TOML row uses. When a check starts failing with "no match", find the new SKU
name with the provider's own tools, fix the helper arguments here, and rerun.
"""

from __future__ import annotations

import argparse
import datetime
import gzip
import html
import json
import re
import sys
import tomllib
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PRICES = ROOT / "definitions" / "prices"
HOURS_PER_MONTH = 730


class Skip(Exception):
    """A row that cannot be checked; the message says why."""


class NoMatch(Exception):
    """The live source no longer has what the mapping asks for."""


# ------------------------------------------------------------------------------ fetching

_cache: dict[str, object] = {}


def fetch(url: str) -> bytes:
    if url not in _cache:
        req = urllib.request.Request(url, headers={"Accept-Encoding": "gzip", "User-Agent": "Mozilla/5.0 prices_check"})
        with urllib.request.urlopen(req, timeout=300) as r:
            body = r.read()
        if body[:2] == b"\x1f\x8b":
            body = gzip.decompress(body)
        _cache[url] = body
    return _cache[url]  # type: ignore[return-value]


def fetch_json(url: str):
    key = "json:" + url
    if key not in _cache:
        _cache[key] = json.loads(fetch(url))
    return _cache[key]


def skip(reason: str):
    """A source for a row that has nothing to check against."""
    def src(region):
        raise Skip(reason)
    return src


def first_nonzero(cands: list[tuple[float, float]], what: str) -> float:
    """cands = [(tier_start, price)]; the price of the lowest tier that is not free."""
    paid = sorted({(t, p) for t, p in cands if p > 0})
    if not paid:
        raise NoMatch(f"no priced entry for {what}")
    lowest = [p for t, p in paid if t == paid[0][0]]
    if len(set(lowest)) > 1:
        raise NoMatch(f"ambiguous {what}: {sorted(set(lowest))}")
    return lowest[0]


# ------------------------------------------------------------------------------ AWS

AWS_NAMES = {"us-east-1": "US East (N. Virginia)", "eu-west-1": "EU (Ireland)", "eu-west-2": "EU (London)"}
# Usage-type prefixes the Price List uses for each region (us-east-1 often has none).
AWS_PREFIXES = {"us-east-1": ("USE1-", "us-east-1-", ""), "eu-west-1": ("EU-", "eu-west-1-"), "eu-west-2": ("EUW2-", "eu-west-2-")}
B0 = "https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/"
BULK = "https://pricing.us-east-1.amazonaws.com"


def _b0_region(path: str, region: str) -> dict:
    name = AWS_NAMES[region]
    url = B0 + path.format(region=urllib.parse.quote(name))
    return fetch_json(url)["regions"].get(name, {})


def ec2(instance: str):
    """EC2 on-demand Linux, shared tenancy, per hour (the EC2 pricing page's own JSON)."""
    def src(region):
        d = _b0_region("ec2/USD/current/ec2-ondemand-without-sec-sel/{region}/Linux/index.json", region)
        hits = {float(v["price"]) for v in d.values() if v.get("Instance Type") == instance}
        if not hits:
            raise NoMatch(f"{instance} is not offered in {region}")
        if len(hits) > 1:
            raise NoMatch(f"ambiguous {instance}: {sorted(hits)}")
        return hits.pop()
    return src


def b0key(path: str, key: str, scale: float = 1.0):
    """An entry of a b0 pricing file looked up by its descriptive key."""
    def src(region):
        d = _b0_region(path, region)
        if key not in d:
            raise NoMatch(f"'{key}' not in {path} for {region}")
        return float(d[key]["price"]) * scale
    return src


def rds(engine: str, instance: str):
    """RDS single-AZ on-demand instance, per hour."""
    def src(region):
        d = _b0_region(f"rds/USD/current/rds-{engine}-ondemand.json", region)
        hits = {float(v["price"]) for k, v in d.items()
                if k.startswith("Database Instance") and "Single-AZ" in k and v.get("Instance Type") == instance}
        if not hits:
            raise NoMatch(f"{instance} ({engine}) is not offered in {region}")
        if len(hits) > 1:
            raise NoMatch(f"ambiguous {instance}: {sorted(hits)}")
        return hits.pop()
    return src


def _bulk_offer(service: str, location: str) -> dict:
    idx = fetch_json(f"{BULK}/offers/v1.0/aws/{service}/current/region_index.json")
    if location not in idx["regions"]:
        raise NoMatch(f"{service} has no offer file for {location}")
    return fetch_json(BULK + idx["regions"][location]["currentVersionUrl"])


def bulk(service: str, usagetype: str, operation: str | None = None, scale: float = 1.0,
         location: str | None = None, prefixed: bool = True):
    """Price List Bulk API: the on-demand price whose usage type (minus the region prefix)
    fully matches `usagetype`; tiered prices take the first tier that is not free.
    `location` reads another offer file (e.g. 'aws-other' for global services)."""
    rx = re.compile(usagetype)

    def src(region):
        offer = _bulk_offer(service, location or region)
        prefixes = AWS_PREFIXES[region] if prefixed and not location else ("",)
        terms = offer["terms"].get("OnDemand", {})
        cands = []
        for sku, p in offer["products"].items():
            a = p.get("attributes", {})
            ut = a.get("usagetype", "")
            if not any(ut.startswith(px) and rx.fullmatch(ut[len(px):]) for px in prefixes):
                continue
            if operation is not None and a.get("operation") != operation:
                continue
            for t in terms.get(sku, {}).values():
                for dim in t["priceDimensions"].values():
                    cands.append((float(dim.get("beginRange", 0) or 0), float(dim["pricePerUnit"]["USD"])))
        return first_nonzero(cands, f"{service} {usagetype} {operation or ''} in {location or region}") * scale
    return src


def cloudfront(region: str, usage: str, scale: float = 1.0) -> float:
    """CloudFront bills by edge geography, not by region: the US edge for us-*, Europe otherwise."""
    geo = "US" if region.startswith("us-") else "EU"
    return bulk("AmazonCloudFront", f"{geo}-{usage}", location="aws-other", prefixed=False, scale=scale)(region)


# table -> row -> source. Units follow the TOML rows (see each table's `unit`).
AWS = {
    "ec2": {sku: ec2(sku) for sku in [
        "t3.micro", "t3.small", "t3.medium", "t3.large", "t3.xlarge", "t3.2xlarge", "m5.large", "m5.xlarge",
        "m5.2xlarge", "m6i.large", "m6i.xlarge", "m6i.2xlarge", "c5.large", "c5.xlarge", "g4dn.xlarge",
        "g4dn.2xlarge", "g5.xlarge", "g5.2xlarge", "g5.4xlarge", "g6.xlarge", "g6.2xlarge", "p3.2xlarge"]},
    "ebs": {
        "gp3": b0key("ec2/USD/current/ebs.json", "Storage General Purpose gp3 GB Mo"),
        "gp2": b0key("ec2/USD/current/ebs.json", "Storage General Purpose gp2 GB Mo"),
    },
    "eks": {"cluster": bulk("AmazonEKS", r"AmazonEKS-Hours:perCluster")},
    "fargate": {
        "vcpu": bulk("AmazonECS", r"Fargate-vCPU-Hours:perCPU"),
        "gb": bulk("AmazonECS", r"Fargate-GB-Hours"),
    },
    "lambda": {
        "duration": bulk("AWSLambda", r"Lambda-GB-Second"),
        "requests": bulk("AWSLambda", r"Request", scale=1e6),
    },
    "rds_postgres": {sku: rds("postgresql", sku) for sku in [
        "db.t3.micro", "db.t3.small", "db.t3.medium", "db.t3.large", "db.t4g.micro", "db.t4g.small",
        "db.t4g.medium", "db.t4g.large", "db.m5.large", "db.m6g.large", "db.m6i.large", "db.m7g.large", "db.r6g.large"]},
    "rds_mysql": {sku: rds("mysql", sku) for sku in [
        "db.t3.micro", "db.t3.small", "db.t3.medium", "db.t3.large", "db.t4g.micro", "db.t4g.small",
        "db.t4g.medium", "db.t4g.large", "db.m5.large", "db.m6g.large", "db.m6i.large", "db.m7g.large", "db.r6g.large"]},
    "rds_storage": {
        "gp3": b0key("rds/USD/current/rds-postgresql-ondemand.json", "Database Storage Single AZ General Purpose-GP3"),
        "gp2": b0key("rds/USD/current/rds-postgresql-ondemand.json", "Database Storage Single AZ General Purpose"),
        "io1": b0key("rds/USD/current/rds-postgresql-ondemand.json", "Database Storage Single AZ Provisioned IOPS"),
        "io2": b0key("rds/USD/current/rds-postgresql-ondemand.json", "Database Storage Single AZ Provisioned IOPS-IO2"),
    },
    # operation CreateCacheCluster:0002 is Redis OSS (0001 Memcached, Valkey is cheaper).
    "elasticache": {sku: bulk("AmazonElastiCache", "NodeUsage:" + re.escape(sku), operation="CreateCacheCluster:0002")
                    for sku in ["cache.t3.micro", "cache.t3.small", "cache.t3.medium", "cache.t4g.micro",
                                "cache.t4g.medium", "cache.m5.large", "cache.m6g.large"]},
    "dynamodb": {
        "write": bulk("AmazonDynamoDB", r"WriteRequestUnits", scale=1e6),
        "read": bulk("AmazonDynamoDB", r"ReadRequestUnits", scale=1e6),
        "storage": bulk("AmazonDynamoDB", r"TimedStorage-ByteHrs"),
    },
    "s3": {
        "standard": bulk("AmazonS3", r"TimedStorage-ByteHrs"),
        "put": bulk("AmazonS3", r"Requests-Tier1", scale=1e3),
        "get": bulk("AmazonS3", r"Requests-Tier2", scale=1e3),
    },
    "efs": {"standard": bulk("AmazonEFS", r"TimedStorage-ByteHrs")},
    "ecr": {"storage": bulk("AmazonECR", r"TimedStorage-ByteHrs")},
    "sqs": {
        "standard": bulk("AWSQueueService", r"Requests-(RBP|Tier1)", scale=1e6),
        "fifo": bulk("AWSQueueService", r"Requests-FIFO-(RBP|Tier1)", scale=1e6),
    },
    "sns": {
        "publish": bulk("AmazonSNS", r"Requests-Tier1", scale=1e6),
        "email": bulk("AmazonSNS", r"DeliveryAttempts-SMTP", scale=1e5),
    },
    "nat_gateway": {
        "hour": b0key("ec2/USD/current/natgateway.json", "Hourly charge for NAT Gateways"),
        "data": b0key("ec2/USD/current/natgateway.json", "Charge for per GB data processed by NatGateways"),
    },
    "public_ipv4": {"address": b0key("vpc/USD/current/vpc.json", "Hourly charge for In use Public IPv4 Addresses per Hrs")},
    "elb": {
        "alb": bulk("AWSELB", r"LoadBalancerUsage", operation="LoadBalancing:Application"),
        "alb_lcu": bulk("AWSELB", r"LCUUsage", operation="LoadBalancing:Application"),
        "nlb": bulk("AWSELB", r"LoadBalancerUsage", operation="LoadBalancing:Network"),
        "nlb_nlcu": bulk("AWSELB", r"LCUUsage", operation="LoadBalancing:Network"),
    },
    "vpc_endpoint": {
        "interface": b0key("vpc/USD/current/vpc.json", "Hourly charge for VPC Endpoints per Hrs"),
        "data": b0key("vpc/USD/current/vpc.json", "Charge for per GB data processed by VPC Endpoints per GB from 0 to 1048576"),
    },
    # CloudFront prices by edge geography: US for us-east-1, Europe for the eu-* regions.
    "cloudfront": {
        "data_out": lambda r: cloudfront(r, "DataTransfer-Out-Bytes"),
        "https": lambda r: cloudfront(r, "Requests-Tier2-HTTPS", scale=1e4),
    },
    "wafv2": {
        "web_acl": bulk("awswaf", r"WebACLV2"),
        "rule": bulk("awswaf", r"RuleV2"),
        "requests": bulk("awswaf", r"RequestV2-Tier1", scale=1e6),
    },
    "route53": {
        "hosted_zone": bulk("AmazonRoute53", r"HostedZone", location="aws-other"),
        "queries": bulk("AmazonRoute53", r"DNS-Queries", location="aws-other", scale=1e6),
    },
    "kms": {
        "key": bulk("awskms", r"KMS-Keys"),
        "requests": bulk("awskms", r"KMS-Requests", scale=1e4),
    },
    "secrets_manager": {
        "secret": bulk("AWSSecretsManager", r"AWSSecretsManager-Secrets"),
        "api": bulk("AWSSecretsManager", r"AWSSecretsManager-?APIRequests?", scale=1e4),
    },
    "cognito": {"essentials_mau": bulk("AmazonCognito", r"CognitoEssentialsMAU")},
    "cloudtrail": {"data_events": bulk("AWSCloudTrail", r"DataEventsRecorded", scale=1e5)},
    "cloudwatch_logs": {
        "ingest": bulk("AmazonCloudWatch", r"DataProcessing-Bytes", operation="PutLogEvents"),
        "storage": bulk("AmazonCloudWatch", r"TimedStorage-ByteHrs"),
    },
    "cloudwatch_alarms": {"standard": bulk("AmazonCloudWatch", r"CW:AlarmMonitorUsage")},
}

# ------------------------------------------------------------------------------ Azure

AZ_API = "https://prices.azure.com/api/retail/prices"
AZ_REGIONS = ("eastus", "westeurope", "uksouth")


def _az_items(filt: str) -> list[dict]:
    key = "az:" + filt
    if key not in _cache:
        url = AZ_API + "?" + urllib.parse.urlencode({"$filter": filt})
        items = []
        while url:
            d = fetch_json(url)
            items += d["Items"]
            url = d.get("NextPageLink")
        _cache[key] = items
    return _cache[key]  # type: ignore[return-value]


def az(filt: str, keep=None, scale: float = 1.0, where: str | None = None):
    """Retail prices API, Consumption meters matching the OData `filt` (plus the region) and
    the optional predicate `keep(item)`; tiered meters take the first tier that is not free.
    `where` reads a non-regional price list ('Global', 'Zone 1', '' for DNS)."""
    def src(region):
        if where is None:
            regs = " or ".join(f"armRegionName eq '{r}'" for r in AZ_REGIONS)
            items = [i for i in _az_items(f"priceType eq 'Consumption' and ({regs}) and {filt}")
                     if i["armRegionName"] == region]
        else:
            items = [i for i in _az_items(f"priceType eq 'Consumption' and armRegionName eq '{where}' and {filt}")]
        items = [i for i in items if keep is None or keep(i)]
        cands = [(float(i.get("tierMinimumUnits") or 0), float(i["retailPrice"])) for i in items]
        return first_nonzero(cands, f"{filt} in {where if where is not None else region}") * scale
    return src


def az_sum(*parts):
    """Sum of weighted sources: az_sum((src, weight), ...)."""
    return lambda region: sum(s(region) * w for s, w in parts)


def vm(sku: str):
    linux = lambda i: not i["productName"].endswith("Windows") and not re.search(r"Spot|Low Priority", i["skuName"])
    return az(f"serviceName eq 'Virtual Machines' and armSkuName eq '{sku}'", keep=linux)


def disk(tier: str):
    return az(f"serviceName eq 'Storage' and skuName eq '{tier} LRS' and meterName eq '{tier} LRS Disk'",
              keep=lambda i: i["productName"].endswith("Managed Disks"))


def flex(engine: str, sku: str):
    """Flexible server compute: burstable SKUs have their own meter; General Purpose is priced
    per vCore on a per-series meter, times the vCores in the SKU name."""
    product = f"Azure Database for {engine} Flexible Server"
    m = re.fullmatch(r"B_Standard_(B\w+)", sku)
    if m:
        return az(f"productName eq '{product} Burstable BS Series Compute' and skuName eq '{m.group(1).upper()}'")
    m = re.fullmatch(r"GP_Standard_D(\d+)(\w*)_(v\d)", sku)
    vcores, series = int(m.group(1)), "D" + m.group(2) + m.group(3)
    # MySQL's Ddsv4 meter is the unnamed "General Purpose Series Compute" (its first GP series).
    name = "General Purpose Series Compute" if (engine == "MySQL" and series == "Ddsv4") else f"General Purpose {series} Series Compute"
    return az(f"productName eq '{product} {name}' and (skuName eq 'vCore' or skuName eq '1 vCore')", scale=vcores)


def redis_(sku: str):
    tier, size = sku.split("_")
    return az(f"serviceName eq 'Redis Cache' and productName eq 'Azure Redis Cache {tier}' and meterName eq '{size} Cache'")


def meter(service: str, meter_name: str, scale: float = 1.0, where: str | None = None, product: str | None = None, sku: str | None = None):
    f = f"serviceName eq '{service}' and meterName eq '{meter_name}'"
    if product:
        f += f" and productName eq '{product}'"
    if sku:
        f += f" and skuName eq '{sku}'"
    return az(f, scale=scale, where=where)


AZURE = {
    "vm": {sku: vm(sku) for sku in [
        "Standard_B1s", "Standard_B1ms", "Standard_B2s", "Standard_B2ms", "Standard_D2s_v3", "Standard_D4s_v3",
        "Standard_D8s_v3", "Standard_D16s_v3", "Standard_D2s_v5", "Standard_D4s_v5", "Standard_NC4as_T4_v3",
        "Standard_NC8as_T4_v3", "Standard_NC16as_T4_v3", "Standard_NC6s_v3", "Standard_NC24ads_A100_v4"]},
    "managed_disk": {t: disk(t) for t in ["S4", "S6", "S10", "S15", "P4", "P6", "P10", "P15", "P20"]},
    "aks": {
        "free": skip("the Free tier has no meter; it costs nothing by definition"),
        "standard": meter("Azure Kubernetes Service", "Standard Uptime SLA"),
    },
    "container_apps": {
        "vcpu": meter("Azure Container Apps", "Standard vCPU Active Usage"),
        "memory": meter("Azure Container Apps", "Standard Memory Active Usage"),
        "requests": meter("Azure Container Apps", "Standard Requests"),
    },
    "functions": {
        "duration": meter("Functions", "Standard Execution Time", product="Functions"),
        "executions": meter("Functions", "Standard Total Executions", product="Functions", scale=1e6 / 10),
        # EP1 = 1 vCPU + 3.5 GiB, billed per vCPU-hour and per GiB-hour.
        "EP1": az_sum((meter("Functions", "Premium vCPU Duration"), 1.0), (meter("Functions", "Premium Memory Duration"), 3.5)),
    },
    "postgres_flexible": {sku: flex("PostgreSQL", sku) for sku in [
        "B_Standard_B1ms", "B_Standard_B2s", "GP_Standard_D2s_v3", "GP_Standard_D4s_v3", "GP_Standard_D2ds_v4",
        "GP_Standard_D4ds_v4", "GP_Standard_D2ds_v5", "GP_Standard_D4ds_v5"]},
    "mysql_flexible": {sku: flex("MySQL", sku) for sku in [
        "B_Standard_B1ms", "B_Standard_B2s", "GP_Standard_D2ds_v4", "GP_Standard_D4ds_v4", "GP_Standard_D2ds_v5", "GP_Standard_D4ds_v5"]},
    "flexible_storage": {
        "postgres": az("productName eq 'Azure Database for PostgreSQL Flex Server Storage' and meterName eq 'Storage Data Stored'"),
        "mysql": az("productName eq 'Azure Database for MySQL Flexible Server Storage' and meterName eq 'Storage Data Stored'"),
    },
    "redis": {sku: redis_(sku) for sku in ["Basic_C0", "Basic_C1", "Basic_C2", "Standard_C0", "Standard_C1", "Standard_C2"]},
    "cosmos": {
        "throughput": meter("Azure Cosmos DB", "100 RU/s", product="Azure Cosmos DB", sku="RUs"),
        "storage": meter("Azure Cosmos DB", "Data Stored", product="Azure Cosmos DB", sku="RUs"),
    },
    "blob": {
        "hot_LRS": meter("Storage", "Hot LRS Data Stored", product="General Block Blob v2"),
        "hot_ZRS": meter("Storage", "Hot ZRS Data Stored", product="General Block Blob v2"),
        "hot_GRS": meter("Storage", "Hot GRS Data Stored", product="General Block Blob v2"),
        "write": meter("Storage", "Hot LRS Write Operations", product="General Block Blob v2"),
        "read": meter("Storage", "Hot Read Operations", product="General Block Blob v2", sku="Hot LRS"),
    },
    "files": {"premium_LRS": meter("Storage", "Premium LRS Provisioned", product="Premium Files")},
    "queue_storage": {
        "capacity": meter("Storage", "LRS Data Stored", product="Queues v2"),
        "operations": meter("Storage", "LRS Class 1 Operations", product="Queues v2"),
    },
    "service_bus": {
        "basic_operations": meter("Service Bus", "Basic Messaging Operations"),
        "standard_base": az("serviceName eq 'Service Bus' and meterName eq 'Standard Base Unit' and unitOfMeasure eq '1/Hour'"),
        "standard_operations": meter("Service Bus", "Standard Messaging Operations"),
        "premium_unit": meter("Service Bus", "Premium Messaging Unit"),
    },
    "acr": {t: meter("Container Registry", f"{t} Registry Unit") for t in ["Basic", "Standard", "Premium"]},
    "nat_gateway": {
        "hour": meter("NAT Gateway", "Standard Gateway", where="Global"),
        "data": meter("NAT Gateway", "Standard Data Processed", where="Global"),
    },
    "public_ip": {"standard_static": meter("Virtual Network", "Standard IPv4 Static Public IP")},
    "load_balancer": {
        "rules": meter("Load Balancer", "Standard Included LB Rules and Outbound Rules", where="Global"),
        "data": meter("Load Balancer", "Standard Data Processed", where="Global"),
    },
    "private_endpoint": {
        "hour": meter("Virtual Network", "Standard Private Endpoint", where="Global"),
        "data": meter("Virtual Network", "Standard Data Processed - Ingress", where="Global"),
    },
    "front_door": {
        "standard_base": meter("Azure Front Door Service", "Standard Base Fees", product="Azure Front Door", where="Zone 1"),
        "premium_base": meter("Azure Front Door Service", "Premium Base Fees", product="Azure Front Door", where="Zone 1"),
        "standard_data_out": meter("Azure Front Door Service", "Standard Data Transfer Out", product="Azure Front Door", where="Zone 1"),
        "premium_data_out": meter("Azure Front Door Service", "Premium Data Transfer Out", product="Azure Front Door", where="Zone 1"),
        "standard_requests": meter("Azure Front Door Service", "Standard Requests", product="Azure Front Door", where="Zone 1"),
        "premium_requests": meter("Azure Front Door Service", "Premium Requests", product="Azure Front Door", where="Zone 1"),
    },
    "dns": {
        "zone": meter("Azure DNS", "Public Zone", where="Zone 1"),
        "queries": meter("Azure DNS", "Public Queries", where="Zone 1"),
    },
    "key_vault": {
        "operations": meter("Key Vault", "Operations", product="Key Vault", sku="Standard"),
        "rotation": meter("Key Vault", "Automated Key Rotation", product="Key Vault", sku="Standard"),
    },
    "log_analytics": {
        "ingest": meter("Log Analytics", "Analytics Logs Data Ingestion"),
        "retention": meter("Log Analytics", "Analytics Logs Data Retention"),
    },
    "monitor_alerts": {"metric_alert": meter("Azure Monitor", "Alerts Metric Monitored")},
}

# ------------------------------------------------------------------------------ GCP

GCP_PAGE = "https://cloud.google.com/"
_REGION_RE = re.compile(r"^[^()]+\(([a-z]+-[a-z]+\d+)\)$")
_MONEY = re.compile(r"\$\s?([0-9][0-9,]*\.?[0-9]*)")
HOURLY, MONTHLY, FLAT = 3, 2, 1  # the page's own usage-type codes on each table


def _strip(s: str) -> str:
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", s))).strip()


def _strings(x):
    if isinstance(x, str):
        yield x
    elif isinstance(x, list):
        for y in x:
            yield from _strings(y)


def _gcp_tables(page: str) -> list[dict]:
    """Every region table on a pricing page: {title, region, usage, header, rows} where rows
    are lists of cell texts (first cell = label)."""
    key = "gcp:" + page
    if key in _cache:
        return _cache[key]  # type: ignore[return-value]
    body = fetch(GCP_PAGE + page).decode("utf-8", "replace")
    out: list[dict] = []

    def is_table(t):
        return isinstance(t, list) and len(t) >= 3 and isinstance(t[2], str) and _REGION_RE.match(t[2]) and isinstance(t[1], list)

    def cells(row):
        row = row[0] if row and isinstance(row[0], list) and row[0] and isinstance(row[0][0], list) else row
        return [" ".join(_strip(s) for s in _strings(c) if _strip(s)) for c in row]

    def walk(x):
        if not isinstance(x, list):
            return
        if len(x) >= 4 and isinstance(x[3], list) and any(is_table(t) for t in x[3]):
            texts = [_strip(s) for s in _strings(x[2]) if _strip(s)] if isinstance(x[2], list) else []
            for t in x[3]:
                if is_table(t):
                    out.append({"title": texts[0] if texts else "", "region": _REGION_RE.match(t[2]).group(1),
                                "usage": (t[3][0] if len(t) > 3 and t[3] else None),
                                "header": [c for c in cells(t[0][0]) if c] if t[0] else [],
                                "rows": [cells(r) for r in t[1]]})
            return
        for y in x:
            walk(y)

    for m in re.finditer(r"AF_initDataCallback\(\{key: '[^']+', hash: '[^']*', data:", body):
        walk(json.loads(body[m.end():body.index(", sideChannel:", m.end())]))
    _cache[key] = out
    return out


def _money(cell: str) -> float | None:
    vals = [float(v.replace(",", "")) for v in _MONEY.findall(cell)]
    paid = [v for v in vals if v > 0]
    return paid[0] if paid else (0.0 if vals else None)


def gtable(page: str, title: str, row: str | None, usage: int = HOURLY, col: str | None = None,
           after: str | None = None, scale: float = 1.0):
    """A price from a region table on a cloud.google.com pricing page. `title` and `row` are
    regexes on the table title and the row's first cell (None = first row); `col` is a regex on
    the header naming the column (default: the first cell with a price); `after` starts the row
    search after the row whose cells match it (for tables with row-spanning group labels)."""
    def src(region):
        for t in _gcp_tables(page):
            if t["region"] != region or t["usage"] != usage or not re.search(title, t["title"]):
                continue
            hi = [i for i, h in enumerate(t["header"]) if col is not None and re.search(col, h)]
            if col is not None and not hi:
                continue  # another table with the same title (e.g. the DWS or Spot one)
            rows = t["rows"]
            if after:
                idx = [i for i, r in enumerate(rows) if any(re.fullmatch(after, c) for c in r)]
                if not idx:
                    continue
                rows = rows[idx[0]:]
            for r in rows:
                joined = " | ".join(r)
                if row is not None and not re.search(row, joined):
                    continue
                if col is not None:
                    shift = len(r) - len(t["header"])  # row-spanning label cells shift the columns
                    cellv = r[hi[0] + shift] if 0 <= hi[0] + shift < len(r) else ""
                    v = _money(cellv)
                else:
                    v = next((m for m in (_money(c) for c in r[1:] if "$" in c) if m is not None), None)
                if v is None:
                    raise NoMatch(f"'{joined[:80]}' has no price in {region} (not offered there?)")
                return v * scale
        raise NoMatch(f"no row /{row}/ in a /{title}/ table for {region} on {page}")
    return src


def _gcp_text(page: str) -> str:
    key = "gcptext:" + page
    if key not in _cache:
        b = fetch(GCP_PAGE + page).decode("utf-8", "replace")
        t = re.sub(r"<script.*?</script>|<style.*?</style>", " ", b, flags=re.S)
        t = re.sub(r"(\s*\|\s*)+", " | ", re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " | ", t))))
        _cache[key] = t
    return _cache[key]  # type: ignore[return-value]


def gtext(page: str, pattern: str, scale: float = 1.0):
    """A price that is the same everywhere, read from the rendered text of a pricing page;
    `pattern` has one group capturing the number. Hourly figures scaled to a month are
    rounded to 6 decimals (the pages publish monthly prices divided by 730)."""
    def src(region):
        m = re.search(pattern, _gcp_text(page))
        if not m:
            raise NoMatch(f"/{pattern}/ not found on {page}")
        return round(float(m.group(1).replace(",", "")) * scale, 6)
    return src


GP = "products/compute/pricing/general-purpose"
ACC = "products/compute/pricing/accelerator-optimized"
_FAMILY = {"e2-micro": "E2 shared-core", "e2-small": "E2 shared-core", "e2-medium": "E2 shared-core",
           "e2-standard": "E2 standard", "n1-standard": "N1 standard", "n2-standard": "N2 standard"}


def gce(sku: str):
    if sku.startswith("g2-"):  # the on-demand table, not the DWS / Spot ones with the same rows
        return gtable(ACC, r".", rf"^{re.escape(sku)} \|", col=r"^Price \(USD\)")
    fam = _FAMILY.get(sku) or _FAMILY[sku.rsplit("-", 1)[0]]
    return gtable(GP, rf"^{fam} machine types", rf"^{re.escape(sku)} \|")


GCP = {
    "gce": {sku: gce(sku) for sku in [
        "e2-micro", "e2-small", "e2-medium", "e2-standard-2", "e2-standard-4", "e2-standard-8", "e2-standard-16",
        "n1-standard-4", "n1-standard-8", "n2-standard-2", "n2-standard-4", "g2-standard-4", "g2-standard-8"]},
    "gpu": {
        "nvidia-tesla-t4": gtable(ACC, r".", r"^NVIDIA T4 \|"),
        "nvidia-tesla-v100": gtable(ACC, r".", r"^NVIDIA V100 \|"),
        "nvidia-tesla-p4": gtable(ACC, r".", r"^NVIDIA P4 \|"),
    },
    "persistent_disk": {
        "pd-standard": gtable("compute/disks-image-pricing", r"^Persistent Disk", r"^Standard provisioned space", MONTHLY),
        "pd-balanced": gtable("compute/disks-image-pricing", r"^Persistent Disk", r"^Balanced provisioned space", MONTHLY),
        "pd-ssd": gtable("compute/disks-image-pricing", r"^Persistent Disk", r"^SSD provisioned space", MONTHLY),
    },
    "gke": {"cluster": gtext("kubernetes-engine/pricing", r"cluster management fee of \$([0-9.]+) per cluster per hour")},
    "cloud_run": {
        "vcpu": gtable("run/pricing", r"^Services \(Instance-based", r"^CPU \(per vCPU-second\)", FLAT),
        "memory": gtable("run/pricing", r"^Services \(Instance-based", r"^Memory \(per GiB-second\)", FLAT),
        # request-based billing: the row's first price is the active-time one
        "vcpu_request": gtable("run/pricing", r"^Services \(Requests-based", r"^CPU \(per vCPU-second\)", FLAT),
        "memory_request": gtable("run/pricing", r"^Services \(Requests-based", r"^Memory \(per GiB-second\)", FLAT),
        "requests": gtable("run/pricing", r"^Services \(Requests-based", r"^Requests \(per 1,000,000\)", FLAT),
    },
    "cloud_sql": {
        "db-f1-micro": gtable("sql/pricing", r"^Instance pricing", r"^db-f1-micro"),
        "db-g1-small": gtable("sql/pricing", r"^Instance pricing", r"^db-g1-small"),
        "vcpu": gtable("sql/pricing", r"^Enterprise edition - General Purpose", r"^vCPUs \|"),
        "memory": gtable("sql/pricing", r"^Enterprise edition - General Purpose", r"^Memory \|"),
        "ssd": gtable("sql/pricing", r"^Storage$", r"^SSD storage capacity", MONTHLY),
    },
    "memorystore": {
        f"basic_{m}": gtable("memorystore/docs/redis/pricing", r".", rf"{m} \(", after="Basic") for m in ["M1", "M2", "M3"]
    },
    "firestore": {
        "read": gtable("firestore/pricing", r"^Pricing by location", r"^Document Reads", col=r"^Default"),
        "write": gtable("firestore/pricing", r"^Pricing by location", r"^Document Writes", col=r"^Default"),
        "storage": gtable("firestore/pricing", r"^Pricing by location", r"^Stored Data", MONTHLY, col=r"^Default"),
    },
    "gcs": {
        "standard": gtable("storage/pricing", r"^Data storage", None, MONTHLY, col=r"^Standard storage"),
        "class_a": gtext("storage/pricing", r"Free operations \| Standard storage \| \$([0-9.]+) \|"),
        "class_b": gtext("storage/pricing", r"Free operations \| Standard storage \| \$[0-9.]+ \| \$[0-9.]+ \| \$([0-9.]+) \|"),
    },
    "filestore": {
        "BASIC_HDD": gtable("filestore/pricing", r"^You are charged for a Filestore instance", r"^Basic HDD", MONTHLY, col=r"^Per GiB"),
        "BASIC_SSD": gtable("filestore/pricing", r"^You are charged for a Filestore instance", r"^Basic SSD", MONTHLY, col=r"^Per GiB"),
        "BASIC_HDD_instance": gtable("filestore/pricing", r"^You are charged for a Filestore instance", r"^Basic HDD", MONTHLY, col=r"^Per Instance"),
    },
    "artifact_registry": {"storage": gtext("artifact-registry/pricing", r"0\.5 gibibyte month and above \| \$([0-9.]+) / 1 gibibyte hour", HOURS_PER_MONTH)},
    "pubsub": {"throughput": gtext("pubsub/pricing", r"the price is \| \$([0-9.]+) per TiB")},
    "cloud_nat": {
        "vm": gtext("nat/pricing", r"\$([0-9.]+) \* the number of VM instances"),
        "data": gtext("nat/pricing", r"the number of VM instances that are using the gateway \| \$([0-9.]+)"),
    },
    "load_balancing": {
        "forwarding_rule": gtable("vpc/network-pricing", r"forwarding rule charges", r"^First 5 forwarding rules"),
        "data": gtable("vpc/network-pricing", r"forwarding rule charges", r"^Inbound data processed by load balancer"),
    },
    "cloud_cdn": {
        "egress": gtext("cdn/pricing", r"North America \| \(including Hawaii\) \| 0 byte to 10 tebibyte \| \$([0-9.]+)"),
        "lookups": gtext("cdn/pricing", r"cache lookup requests \| \$([0-9.]+) / 10,000"),
    },
    "cloud_armor": {
        "policy": gtext("armor/pricing", r"Security policies \| \$([0-9.]+) / 1 hour", HOURS_PER_MONTH),
        "rule": gtext("armor/pricing", r"Rules \| \$([0-9.]+) / 1 hour", HOURS_PER_MONTH),
        "requests": gtext("armor/pricing", r"Requests \(globally scoped security policies\) \| \$([0-9.]+) / 1,000,000"),
    },
    "cloud_dns": {
        "zone": gtext("dns/pricing", r"Managed zones \| 0 month to 25 month \| \$([0-9.]+) / 1 hour", HOURS_PER_MONTH),
        "queries": gtext("dns/pricing", r"Regular queries \| 0 count to 1,000,000,000 count \| \$([0-9.]+) / 1,000,000"),
    },
    "secret_manager": {
        "version": gtext("secret-manager/pricing", r"Active secret versions \| 0 month to 6 month \|[^|]*\| 6 month and above \| \$([0-9.]+) / 1 hour", HOURS_PER_MONTH),
        "access": gtext("secret-manager/pricing", r"Access operations \|[^|]*\|[^|]*\| 10,000 count and above \| \$([0-9.]+) / 10,000"),
    },
    "cloud_kms": {
        "key_version": gtext("kms/pricing", r"Active symmetric AES-256 and HMAC key versions \| 2 \| \$([0-9.]+) / 1 hour", HOURS_PER_MONTH),
        "operations": gtext("kms/pricing", r"Key operations: Cryptographic \| 4,5 \| \$([0-9.]+) / 10,000"),
    },
    "logging": {
        "ingest": gtext("stackdriver/pricing", r"except for vended network logs\. \| \$([0-9.]+)/GiB"),
        "retention": gtext("stackdriver/pricing", r"\$([0-9.]+) per GiB per month for logs retained more than 30 days"),
    },
    "monitoring": {"condition": lambda r: gcp_alerting()},
}


def gcp_alerting() -> float:
    """Cloud Monitoring alerting is free until the start date the page announces ("Starting no
    sooner than September 1, 2027, ..."); after that, the per-metric-reference monthly price."""
    t = _gcp_text("stackdriver/pricing")
    m = re.search(r"Starting no sooner than ([A-Z][a-z]+ \d+, \d{4}), Cloud Monitoring will begin charging", t)
    if m and datetime.datetime.strptime(m.group(1), "%B %d, %Y").date() > datetime.date.today():
        return 0.0
    return gtext("stackdriver/pricing", r"\$([0-9.]+) per month for each metric reference")("*")

# ------------------------------------------------------------------------------ driver

PROVIDERS = {"aws": AWS, "azure": AZURE, "gcp": GCP}


def check(provider: str, tolerance: float, verbose: bool) -> tuple[int, int, int, int]:
    book = tomllib.loads((PRICES / f"{provider}.toml").read_text(encoding="utf-8"))
    mapping = PROVIDERS[provider]
    regions = book["regions"]
    ok = diff = skip = err = 0
    for tname, table in book["tables"].items():
        rows_map = mapping.get(tname, {})
        for rname, row in table["rows"].items():
            prices = {k: v for k, v in row.items() if k != "unit"}
            src = rows_map.get(rname)
            if src is None:
                print(f"{provider:5} {tname:18} {rname:26} {'':12} SKIP  no live source mapped")
                skip += 1
                continue
            cols = regions if "*" in prices else [r for r in regions if r in prices]
            missing = [r for r in regions if r not in prices and "*" not in prices]
            for region in cols + missing:
                bundled = prices.get(region, prices.get("*"))
                label = f"{provider:5} {tname:18} {rname:26} {region:12}"
                try:
                    live = src(region)
                except Skip as e:
                    print(f"{label} SKIP  {e}")
                    skip += 1
                    continue
                except NoMatch as e:
                    if bundled is None:
                        if verbose:
                            print(f"{label} ok    not offered, and not in the list ({e})")
                        ok += 1
                    else:
                        print(f"{label} ERROR {e}")
                        err += 1
                    continue
                except Exception as e:  # network trouble, a page that changed shape, ...
                    print(f"{label} ERROR {type(e).__name__}: {e}")
                    err += 1
                    continue
                if bundled is None:
                    print(f"{label} DIFF  missing from the list; live {live:.6g}")
                    diff += 1
                    continue
                rel = 0.0 if live == bundled else (abs(bundled - live) / live if live else float("inf"))
                col = "*" if "*" in prices and region not in prices else ""
                if rel > tolerance:
                    print(f"{label} DIFF  bundled{col} {bundled:.6g}  live {live:.6g}  ({(bundled - live) / live * 100 if live else float('inf'):+.1f}%)")
                    diff += 1
                else:
                    if verbose:
                        print(f"{label} ok    bundled{col} {bundled:.6g}  live {live:.6g}")
                    ok += 1
    return ok, diff, skip, err


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--provider", choices=sorted(PROVIDERS), help="check one provider only")
    ap.add_argument("--tolerance", type=float, default=2.0, help="allowed difference in percent (default 2)")
    ap.add_argument("--verbose", "-v", action="store_true", help="print matching rows too")
    args = ap.parse_args()
    total = [0, 0, 0, 0]
    for p in [args.provider] if args.provider else list(PROVIDERS):
        res = check(p, args.tolerance / 100.0, args.verbose)
        print(f"-- {p}: {res[0]} ok, {res[1]} differ, {res[2]} skipped, {res[3]} errors")
        total = [a + b for a, b in zip(total, res)]
    print(f"== {total[0]} ok, {total[1]} differ, {total[2]} skipped, {total[3]} errors (tolerance {args.tolerance}%)")
    return 1 if total[1] or total[3] else 0


if __name__ == "__main__":
    sys.exit(main())
