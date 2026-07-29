# Upstream attribution

Project OpenSource was designed with two read-only reference archives. No upstream
source code has been copied into `app/` at this stage. Architectural ideas are
reimplemented behind a new canonical runtime.

## OpenAI Codex CLI

- Repository: <https://github.com/openai/codex>
- Archive: `C:\Users\HP\Downloads\codex-main.zip`
- SHA-256: `0879b1af70f0fe628a22ceb2b3aad2be0ceca92e9df32db6c880c16dace6dba7`
- Observable package versions: `codex-cli` `0.0.0-dev`; Rust workspace `0.0.0`
- Exact Git commit: unavailable because the ZIP contains no `.git` metadata
- License: Apache-2.0
- Notice: `upstream/codex/NOTICE`
- Read-only reference destination: `upstream/codex`
- Files copied: none
- Files adapted: none
- Concepts reimplemented: app-server/client boundary, persistent agent topology,
  event-oriented runtime, approval/sandbox separation, Ratatui client, and
  resumable session concepts
- Major modifications: unified vocabulary, provider-neutral request contract,
  structured completion, per-agent tool policy, task DAG, workspace ownership,
  and one SQLite event ledger

The Codex archive contains a Unix symbolic link at
`codex-rs/vendor/bubblewrap/LICENSE -> COPYING`. Windows ZIP extraction cannot
preserve that link reliably, so the extracted reference tree materializes
`LICENSE` with the contents of `COPYING`. The original archive is unchanged.

Codex's `NOTICE` also attributes Ratatui under the MIT license. Project
OpenSource uses Ratatui as a dependency and will preserve the dependency's
license metadata in release packaging.

## Google Gemini CLI

- Repository: <https://github.com/google-gemini/gemini-cli>
- Archive: `C:\Users\HP\Downloads\gemini-cli-main.zip`
- SHA-256: `36332787e101521f5c5f4f3e12713c5b1ccf34ed95f8f618847862837cd2476c`
- Observable package version: `0.54.0-nightly.20260722.gf743ab579`
- Commit hint embedded in version: `f743ab579` (not independently verifiable
  without Git metadata)
- License: Apache-2.0
- Read-only reference destination: `upstream/gemini-cli`
- Files copied: none
- Files adapted: none
- Concepts reimplemented: Markdown agent definitions, isolated tool registries,
  mandatory structured completion, lazy skills, scheduler semantics, browser
  specialist boundaries, and remote-agent capability contracts
- Major modifications: concepts are represented in the Rust canonical runtime;
  Gemini-specific model/function-call assumptions are not retained

## Updating this record

Any future copied or adapted file must add a row with its exact upstream path,
license, Project OpenSource destination, modification summary, and whether it is
copied or adapted. Dependency lockfiles and generated code are not evidence of
source provenance by themselves.
