"""Local OpenRPC references and annotation-only aliases used by all Python tools."""
import copy


def expand_common(source):
    """Expand top-level aliases for generators that require named inline records."""
    def expand(schema, seen=()):
        branches = schema.get("allOf", [])
        if len(branches) != 1 or set(branches[0]) != {"$ref"}:
            return copy.deepcopy(schema)
        ref = branches[0]["$ref"]
        if not ref.startswith("#/definitions/") or any(k != "allOf" and k not in ("description", "title") and not k.startswith("x-") for k in schema):
            return copy.deepcopy(schema)
        if ref in seen:
            raise ValueError(f"cyclic schema alias: {ref}")
        target = source
        for part in ref[2:].split("/"):
            target = target[part.replace("~1", "/").replace("~0", "~")]
        result = expand(target, (*seen, ref))
        result = {k:v for k,v in result.items() if k not in ("description", "title") and not k.startswith("x-rust-")}
        result.update({k:copy.deepcopy(v) for k,v in schema.items() if k != "allOf"})
        return result
    result = copy.deepcopy(source)
    result["definitions"] = {name:expand(schema) for name,schema in source["definitions"].items()}
    return result


def expand_document(source, common):
    """Resolve OpenRPC object slots, leaving JSON Schema references intact."""
    def dereference(value, owner):
        seen = set()
        while "$ref" in value:
            ref = value["$ref"]
            if ref.startswith("common.json#/"):
                owner, pointer = common, ref[len("common.json#/"):]
            elif ref.startswith("#/"):
                pointer = ref[2:]
            else:
                raise ValueError(f"unexpected OpenRPC reference: {ref}")
            key = (id(owner), pointer)
            if key in seen:
                raise ValueError(f"cyclic OpenRPC reference: {ref}")
            seen.add(key)
            value = owner
            for part in pointer.split("/"):
                value = value[int(part)] if isinstance(value, list) else value[part.replace("~1", "/").replace("~0", "~")]
        return copy.deepcopy(value), owner

    def rebase(value):
        if isinstance(value, list):
            return [rebase(v) for v in value]
        if isinstance(value, dict):
            return {k:("common.json"+v if k=="$ref" and v.startswith("#/") else rebase(v)) for k,v in value.items()}
        return value

    def entry(value):
        row, owner = dereference(value, source)
        for field in ("params", "errors"):
            if field in row:
                row[field] = [slot(v, owner) for v in row[field]]
        if "result" in row:
            row["result"] = slot(row["result"], owner)
        return rebase(row) if owner is common else row

    def slot(value, owner):
        value, owner = dereference(value, owner)
        return rebase(value) if owner is common else value

    result = copy.deepcopy(source)
    for field in ("methods", "x-notifications", "x-completions"):
        if field in result:
            result[field] = [entry(v) for v in result[field]]
    return result
