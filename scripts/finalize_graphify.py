"""Finalize Graphify manifests and cumulative token-cost metadata."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path

from graphify.detect import save_manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("project", type=Path)
    args = parser.parse_args()

    project = args.project.resolve()
    output = project / "graphify-out"
    detection = json.loads(
        output.joinpath(".graphify_detect.json").read_text(encoding="utf-8")
    )
    extraction = json.loads(
        output.joinpath(".graphify_extract.json").read_text(encoding="utf-8")
    )

    original_directory = Path.cwd()
    try:
        __import__("os").chdir(project)
        save_manifest(detection["files"])
    finally:
        __import__("os").chdir(original_directory)

    cost_path = output / "cost.json"
    if cost_path.exists():
        cost = json.loads(cost_path.read_text(encoding="utf-8"))
    else:
        cost = {"runs": [], "total_input_tokens": 0, "total_output_tokens": 0}

    input_tokens = extraction.get("input_tokens", 0)
    output_tokens = extraction.get("output_tokens", 0)
    cost["runs"].append(
        {
            "date": datetime.now(timezone.utc).isoformat(),
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "files": detection.get("total_files", 0),
            "extraction": "deterministic_ast",
        }
    )
    cost["total_input_tokens"] += input_tokens
    cost["total_output_tokens"] += output_tokens
    cost_path.write_text(json.dumps(cost, indent=2), encoding="utf-8")

    print(
        f"{project.name}: {input_tokens:,} input tokens, "
        f"{output_tokens:,} output tokens"
    )


if __name__ == "__main__":
    main()
