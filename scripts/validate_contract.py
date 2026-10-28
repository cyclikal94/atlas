"""Validate the canonical API schemas and examples in current documentation."""
import hashlib
import json
import re
from pathlib import Path
from jsonschema import Draft202012Validator, FormatChecker
from openapi_spec_validator import validate

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "api/openapi.json"
# Strict MAJOR.MINOR.PATCH. Keep in step with crates/server/build.rs and the /health schema pattern.
API_VERSION = r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$"
RECORD = "ATLAS_RECORD_COMPATIBILITY=1 cargo test --locked -p atlas-server --test api compatibility"
document = json.loads(CONTRACT.read_text())
validate(document)
count = 0

def check_api_version():
    """The contract version is the API version reported by /health and recorded with its hash."""
    version = document["info"]["version"]
    if not re.fullmatch(API_VERSION, version):
        raise SystemExit(f"info.version {version!r} must be MAJOR.MINOR.PATCH (numeric, no leading zeros)")
    health = document["paths"]["/health"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
    if health.get("required") != ["status", "api_version"] or health["properties"]["api_version"].get("pattern") != API_VERSION:
        raise SystemExit("The /health 200 schema must require status and api_version, and api_version must use the info.version pattern")
    try:
        baseline = json.loads((ROOT / "api/compatibility.json").read_text())
    except FileNotFoundError:
        raise SystemExit(f"api/compatibility.json is missing. Record it with: {RECORD}") from None
    digest = hashlib.sha256(CONTRACT.read_bytes()).hexdigest()
    if baseline.get("api_version") != version or baseline.get("contract_sha256") != digest:
        raise SystemExit(
            f"api/openapi.json (info.version {version}, sha256 {digest}) does not match api/compatibility.json "
            f"({baseline.get('api_version')}, {baseline.get('contract_sha256')}). Any contract edit needs a greater "
            f"info.version (docs/api.md, 'API version and compatibility'), then record with: {RECORD}")

check_api_version()

def check_sync_bounds():
    """A delta page at the /sync limit ceiling must be valid against SyncPage (BE-B1)."""
    [limit] = [p["schema"] for p in document["paths"]["/sync"]["get"]["parameters"] if p["name"] == "limit"]
    schemas = document["components"]["schemas"]
    page = schemas["SyncPage"]["properties"]["batches"]["maxItems"]
    changes = schemas["Batch"]["properties"]["changes"]["maxItems"]
    if limit.get("minimum") != 1 or not limit["default"] <= limit["maximum"]:
        raise SystemExit(f"/sync limit must have minimum 1 and default <= maximum, got {limit}")
    if limit["maximum"] != page:
        raise SystemExit(f"/sync limit maximum {limit['maximum']} must equal SyncPage.batches.maxItems {page}")
    if changes < limit["maximum"]:
        raise SystemExit(f"Batch.changes.maxItems {changes} must hold a full snapshot page of {limit['maximum']} resources")

check_sync_bounds()

def check_example(name, value):
    schema = {**document, "$ref": f"#/components/schemas/{name}"}
    Draft202012Validator(schema, format_checker=FormatChecker()).validate(value)

for name, schema in document["components"]["schemas"].items():
    Draft202012Validator.check_schema(schema)
    for example in schema.get("examples", []):
        check_example(name, example)
        count += 1
for path in (ROOT / "docs").glob("*.md"):
    # Superseded draft examples are removed with the documentation cleanup.
    for name, raw in re.findall(r"<!-- experimental-schema: ([A-Za-z0-9_]+) -->\s*```json\n(.*?)\n```", path.read_text(), re.DOTALL):
        check_example(name, json.loads(raw))
        count += 1
# The payloads the compatibility probe records come from the real builder, so they must satisfy
# the schema; this keeps the schema and the code from drifting apart.
recorded = json.loads((ROOT / "api/compatibility.json").read_text())["behaviour"]["web_push_payload"]
for shape in ("declarative", "identifiers_only"):
    check_example("WebPushPayload", recorded[shape]["payload"])
    count += 1
print(f"Validated OpenAPI, {len(document['components']['schemas'])} schemas and {count} examples")
