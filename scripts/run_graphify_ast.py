"""Build deterministic Graphify AST artifacts for a selected source directory."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from graphify.detect import detect
from graphify.extract import extract


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    source = args.source.resolve()
    output = args.output.resolve()
    graphify_out = output / "graphify-out"
    graphify_out.mkdir(parents=True, exist_ok=True)

    detection = detect(source)
    graphify_out.joinpath(".graphify_detect.json").write_text(
        json.dumps(detection, indent=2), encoding="utf-8"
    )
    code_files = [
        Path(path)
        for path in detection.get("files", {}).get("code", [])
    ]
    ast = (
        extract(code_files, cache_root=output)
        if code_files
        else {"nodes": [], "edges": [], "input_tokens": 0, "output_tokens": 0}
    )
    semantic = {
        "nodes": [],
        "edges": [],
        "hyperedges": [],
        "input_tokens": 0,
        "output_tokens": 0,
    }
    merged = {
        "nodes": ast.get("nodes", []),
        "edges": ast.get("edges", []),
        "hyperedges": [],
        "input_tokens": 0,
        "output_tokens": 0,
    }
    graphify_out.joinpath(".graphify_ast.json").write_text(
        json.dumps(ast, indent=2), encoding="utf-8"
    )
    graphify_out.joinpath(".graphify_semantic.json").write_text(
        json.dumps(semantic, indent=2), encoding="utf-8"
    )
    graphify_out.joinpath(".graphify_extract.json").write_text(
        json.dumps(merged, indent=2), encoding="utf-8"
    )
    graphify_out.joinpath(".graphify_python").write_text(
        __import__("sys").executable, encoding="utf-8"
    )
    graphify_out.joinpath(".graphify_root").write_text(
        str(source), encoding="utf-8"
    )
    print(
        f"{source}: {len(merged['nodes'])} nodes, "
        f"{len(merged['edges'])} edges"
    )


if __name__ == "__main__":
    main()
