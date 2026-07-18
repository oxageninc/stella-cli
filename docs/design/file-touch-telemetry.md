# File-touch telemetry

Every file the agent touches during a session produces exactly one
session-level record. The tracker lives at the tool-dispatch boundary
(`stella_tools::ToolRegistry::execute`), so every successful `read_file`,
`write_file`, `edit_file`, and `delete_file` call is recorded — reads
included, not only changes. File operations done through `bash` are opaque to
the tracker (not attributable to a path), which is why the CRUD tools exist
and the system prompt steers agents toward them.

The payload is exposed as `ToolRegistry::file_touch_telemetry()`
(`stella_tools::file_touch::FileTouchTelemetry`), emitted in the `--format
json` session summary under `files_touched`, and persisted per execution in
`.stella/store.db` (`files_touched` table: one row per normalized path,
`UNIQUE (execution_id, path)`).

## CRUD event semantics

Only these four uppercase event identifiers exist:

| Event | Meaning | `lines_added` | `lines_removed` |
| --- | --- | --- | --- |
| `C` | File went from nonexistent to existing | full line count of the new file | 0 |
| `R` | Content or metadata successfully read | 0 | 0 |
| `U` | Existing file's content changed | from a line diff of pre- vs post-write content | from the same diff |
| `D` | File went from existing to nonexistent | 0 | full line count immediately before deletion |

Rules the payload guarantees (checked by `FileTouchTelemetry::validate`):

- Failed operations never create an event.
- Paths are normalized before aggregation: workspace-relative, `/`
  separators, `.`/`..` collapsed; paths escaping the workspace are rejected
  by the tools themselves and never reach the ledger.
- One record per normalized path; `events` is the chronological audit log
  with one entry per touch and is never deduplicated; `crud_events` is
  deduplicated in first-occurrence order.
- Per file, the top-level `lines_added`/`lines_removed` equal the sums over
  its `events`.
- Ledger updates are atomic (one mutex guards event append + aggregate
  update), so concurrent tool execution loses nothing.

Line counts use `str::lines()` semantics: an empty file is 0 lines and a
trailing newline adds none. Non-UTF-8 content is counted through a lossy
conversion. Update diffs are minimal line edit scripts (LCS); above a size
cap the diff falls back to the replace-everything upper bound.

## JSON Schema

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Agent File-Touch Telemetry",
  "type": "object",
  "additionalProperties": false,
  "required": ["files_touched"],
  "properties": {
    "files_touched": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": [
          "path",
          "crud_events",
          "lines_added",
          "lines_removed",
          "events"
        ],
        "properties": {
          "path": {
            "type": "string",
            "minLength": 1,
            "description": "Normalized repository-relative POSIX filepath."
          },
          "crud_events": {
            "type": "array",
            "minItems": 1,
            "uniqueItems": true,
            "items": {
              "type": "string",
              "enum": ["C", "R", "U", "D"]
            },
            "description": "Unique CRUD events for this file, ordered by first occurrence."
          },
          "lines_added": {
            "type": "integer",
            "minimum": 0,
            "description": "Sum of lines added by C and U events in this session."
          },
          "lines_removed": {
            "type": "integer",
            "minimum": 0,
            "description": "Sum of lines removed by U and D events in this session."
          },
          "events": {
            "type": "array",
            "minItems": 1,
            "description": "Chronological audit log; one item for every file touch.",
            "items": {
              "type": "object",
              "additionalProperties": false,
              "required": [
                "event",
                "reason",
                "lines_added",
                "lines_removed"
              ],
              "properties": {
                "event": {
                  "type": "string",
                  "enum": ["C", "R", "U", "D"]
                },
                "reason": {
                  "type": "string",
                  "minLength": 1,
                  "description": "Human-readable explanation of why the agent touched the file."
                },
                "lines_added": {
                  "type": "integer",
                  "minimum": 0
                },
                "lines_removed": {
                  "type": "integer",
                  "minimum": 0
                }
              }
            }
          }
        }
      }
    }
  }
}
```

The `reason` string comes from the optional `reason` input field each file
tool advertises; when the model omits it, a per-op default (e.g. `"file read
(no reason given)"`) keeps the field non-empty.

## Example

```json
{
  "files_touched": [
    {
      "path": "stella-tui/src/render.rs",
      "crud_events": ["R", "U"],
      "lines_added": 18,
      "lines_removed": 4,
      "events": [
        {
          "event": "R",
          "reason": "Inspect media-event rendering logic",
          "lines_added": 0,
          "lines_removed": 0
        },
        {
          "event": "U",
          "reason": "Add media-event status indicator rendering",
          "lines_added": 18,
          "lines_removed": 4
        }
      ]
    }
  ]
}
```
