#!/usr/bin/env python3
"""Best-effort OpenAPI 3.1 -> 3.0 downconverter.

RESTler's spec parser (NJsonSchema) only understands OpenAPI 3.0 / Swagger 2.0,
but utoipa emits 3.1. This rewrites the 3.1-only constructs (nullable type arrays,
oneOf/anyOf-with-null for Option<T>, numeric exclusiveMin/Max, const, schema-level
examples, boolean items, 2020-12 keywords) into their 3.0 equivalents so RESTler
can compile a grammar. Lossy but adequate for fuzzing. Usage: <in.json> <out.json>
"""
import json
import sys


def conv(node):
    if isinstance(node, list):
        return [conv(x) for x in node]
    if not isinstance(node, dict):
        return node

    node = {k: conv(v) for k, v in node.items()}

    # type: [..., "null"]  ->  type: <non-null> + nullable: true
    t = node.get("type")
    if isinstance(t, list):
        if "null" in t:
            node["nullable"] = True
        non_null = [x for x in t if x != "null"]
        if non_null:
            node["type"] = non_null[0]
        else:
            node.pop("type", None)

    # oneOf/anyOf containing {"type":"null"}  ->  drop null member + nullable: true
    for comb in ("oneOf", "anyOf"):
        members = node.get(comb)
        if isinstance(members, list):
            has_null = any(isinstance(m, dict) and m.get("type") == "null" for m in members)
            kept = [m for m in members if not (isinstance(m, dict) and m.get("type") == "null")]
            if has_null:
                node["nullable"] = True
            if len(kept) == 1:
                only = kept[0]
                node.pop(comb)
                if "$ref" in only and node.get("nullable"):
                    node["allOf"] = [only]  # 3.0 can't put nullable beside $ref directly
                else:
                    node.update(only)
            elif kept:
                node[comb] = kept

    # standalone {"type": "null"}
    if node.get("type") == "null":
        node.pop("type")
        node["nullable"] = True

    # numeric exclusiveMinimum/Maximum (3.1)  ->  boolean form (3.0)
    for ex, base in (("exclusiveMinimum", "minimum"), ("exclusiveMaximum", "maximum")):
        v = node.get(ex)
        if isinstance(v, (int, float)) and not isinstance(v, bool):
            node[base] = v
            node[ex] = True

    # const  ->  single-value enum
    if "const" in node:
        node["enum"] = [node.pop("const")]

    # schema-level examples (array)  ->  example (singular). Media-type "examples"
    # is a MAP and stays valid in 3.0, so only touch the array form.
    if isinstance(node.get("examples"), list):
        if node["examples"]:
            node.setdefault("example", node["examples"][0])
        node.pop("examples")

    # boolean items  ->  object schema
    if isinstance(node.get("items"), bool):
        node["items"] = {} if node["items"] else {"not": {}}

    # drop JSON-Schema-2020-12 keywords 3.0 doesn't model
    for k in ("prefixItems", "$schema", "unevaluatedProperties", "$comment",
              "contentMediaType", "contentEncoding"):
        node.pop(k, None)

    return node


def main():
    src, dst = sys.argv[1], sys.argv[2]
    with open(src) as f:
        spec = json.load(f)
    spec["openapi"] = "3.0.3"
    out = conv(spec)
    with open(dst, "w") as f:
        json.dump(out, f)
    print(f"converted {src} -> {dst}")


if __name__ == "__main__":
    main()
