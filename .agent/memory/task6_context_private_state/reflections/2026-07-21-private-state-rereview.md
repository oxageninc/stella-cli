# Private-state re-review reflection

The review exposed a boundary mismatch: writers used the secure private-state
resolver, while several read-only and OAuth construction paths still assembled
legacy paths or erased resolver failures. Centralizing the boundary is not
complete until every entry point resolves before delegating to lower-level
readers and the resolver's error remains distinguishable from ordinary absence.

The strongest regression witnesses exercise observable filesystem behavior:
safe legacy files migrate, unsafe layouts remain untouched with actionable
errors, and `git check-ignore` proves a newly resolved token path cannot be
staged. These witnesses caught gaps that path-string assertions did not.

For similar persistence changes, inventory constructors and read-only commands
separately from writers, make path resolution fallible end to end, and test the
consumer-visible warning/error behavior in addition to the storage primitive.
