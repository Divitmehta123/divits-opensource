"""Build Graphify report and graph JSON from deterministic extraction data."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from graphify.analyze import god_nodes, suggest_questions, surprising_connections
from graphify.build import build_from_json
from graphify.cluster import cluster, score_all
from graphify.export import to_json
from graphify.report import generate


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("project", type=Path)
    parser.add_argument("--labels", type=Path)
    args = parser.parse_args()

    project = args.project.resolve()
    output = project / "graphify-out"
    extraction = json.loads(
        output.joinpath(".graphify_extract.json").read_text(encoding="utf-8")
    )
    detection = json.loads(
        output.joinpath(".graphify_detect.json").read_text(encoding="utf-8")
    )

    graph = build_from_json(extraction)
    if graph.number_of_nodes() == 0:
        raise SystemExit("Graphify extraction produced an empty graph")
    communities = cluster(graph)
    cohesion = score_all(graph, communities)
    labels = {community_id: f"Community {community_id}" for community_id in communities}
    if args.labels:
        provided = json.loads(args.labels.read_text(encoding="utf-8"))
        labels.update({int(key): value for key, value in provided.items()})

    gods = god_nodes(graph)
    surprises = surprising_connections(graph, communities)
    questions = suggest_questions(graph, communities, labels)
    tokens = {
        "input": extraction.get("input_tokens", 0),
        "output": extraction.get("output_tokens", 0),
    }
    root = output.joinpath(".graphify_root").read_text(encoding="utf-8").strip()
    report = generate(
        graph,
        communities,
        cohesion,
        labels,
        gods,
        surprises,
        detection,
        tokens,
        root,
        suggested_questions=questions,
    )
    output.joinpath("GRAPH_REPORT.md").write_text(report, encoding="utf-8")
    output.joinpath(".graphify_analysis.json").write_text(
        json.dumps(
            {
                "communities": {str(key): value for key, value in communities.items()},
                "cohesion": {str(key): value for key, value in cohesion.items()},
                "gods": gods,
                "surprises": surprises,
                "questions": questions,
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    output.joinpath(".graphify_labels.json").write_text(
        json.dumps({str(key): value for key, value in labels.items()}, indent=2),
        encoding="utf-8",
    )
    to_json(graph, communities, output / "graph.json")
    print(
        f"{project.name}: {graph.number_of_nodes()} nodes, "
        f"{graph.number_of_edges()} edges, {len(communities)} communities"
    )


if __name__ == "__main__":
    main()
