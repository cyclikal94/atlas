"""Validate the canonical API schemas and examples in current documentation."""
import json
import re
from pathlib import Path
from jsonschema import Draft202012Validator, FormatChecker
from openapi_spec_validator import validate

ROOT = Path(__file__).resolve().parents[1]
document = json.loads((ROOT / "api/openapi.json").read_text())
validate(document)
count = 0

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
print(f"Validated OpenAPI, {len(document['components']['schemas'])} schemas and {count} examples")
