# x4c-lsp

A target-agnostic language server for P4-16. It speaks the Language Server
Protocol over stdio and logs to stderr (in Helix: `:log-open`).

Features:

- Diagnostics while typing: lexer, preprocessor (missing includes,
  `#error`, unbalanced conditionals), syntax errors with recovery, and name
  resolution (unknown names, states, match kinds, and struct/header
  members; duplicate declarations). Code skipped by `#if`/`#ifdef` is
  reported as faded "unnecessary" hints.
- Go to definition, declaration, and type definition, including through
  `#include` paths and macro names. A fragment included in several places
  offers each declaration it resolves to.
- Hover with the declaration, its container, and its doc comment (the
  comment above it, or the one trailing it on the same line).
- Find references, document highlight (with writes marked), and rename.
- Completion of members after `.` and of names in scope, with keywords and
  macros.
- Signature help for actions, functions, extern methods and constructors,
  and parser/control `apply`, listing every overload.
- Document symbols (nested) and workspace symbols.

The frontend is independent of x4c's code generator and of any target.
Architecture headers such as `core.p4`, `v1model.p4`, `tna.p4`, and
`softnpu.p4` are ordinary includes. For x4c's own dialect, the names x4c
declares implicitly (`NoAction`, the `exact`/`ternary`/`lpm`/`range` match
kinds, and the `SoftNPU` package) resolve when a program does not declare
them itself.

P4 programs are analyzed as a whole, starting from a *root* file (the one
that instantiates `main`) and following its `#include`s. This is what makes
fragment files work: a file such as dendrite's `route_selector.p4`, which is
included inside a control body, is analyzed in the context of the program
that includes it.

## Install

```sh
cargo install --path lsp
x4c-lsp --version
```

`x4c-lsp check [-I DIR]... [-D NAME[=VALUE]]... FILE...` runs the same
analysis from the command line and prints diagnostics, which is handy in CI:

```sh
x4c-lsp check -I dpd/p4 -I $SDE/install/share/p4c/p4include \
    -D __TARGET_TOFINO__=2 dpd/p4/sidecar.p4
```

## Configuration: `x4c-lsp.toml`

The server looks for `x4c-lsp.toml` in the directory of the opened file and
its parents, up to the workspace root. Paths are relative to the directory
containing the file; `~`, `$VAR`, and `${VAR}` are expanded.

```toml
# Include directories and defines for every root.
include = ["$SDE/install/share/p4c/p4include"]
define = ["__TARGET_TOFINO__=2"]   # NAME (defined as 1) or NAME=VALUE

# Compilation roots, tried in order. A file belonging to several roots is
# analyzed with the first one that includes it.
[[root]]
file = "dpd/p4/sidecar.p4"
include = ["dpd/p4"]               # searched before the global list

[[root]]                            # the same program built with multicast
file = "dpd/p4/sidecar.p4"
include = ["dpd/p4"]
define = ["MULTICAST"]
```

The example above is for [dendrite](https://github.com/oxidecomputer/dendrite):
place it at the repository root. To analyze only the multicast build, keep
just the second root.

Root `include`s are searched first, then the global `include`s. Root
`define`s override global ones with the same name.

### Without a configuration file

- **Include directories** default to `$P4C_INCLUDE` (colon-separated) plus
  whichever of these exist: `/usr/share/p4c/p4include`,
  `/usr/local/share/p4c/p4include`, `$SDE_INSTALL/share/p4c/p4include`,
  `$SDE/install/share/p4c/p4include`, and the `tofino` and `tofino2`
  subdirectories of each. These defaults apply whenever no global `include`
  is configured.
- **Roots** are inferred: `.p4` files in the opened file's directory that
  instantiate `main` (e.g. `V1Switch(...) main;`) are candidate roots, with
  their directory on the include path. The first candidate whose include
  closure contains the opened file is used; otherwise the file is analyzed on
  its own.

### Editor settings

The same schema can be passed as JSON in the client's initialization options
or `workspace/didChangeConfiguration` settings. There, relative paths are
resolved against the workspace root. A project's `x4c-lsp.toml` takes
precedence: its `include`s come first, its `define`s win, and its roots
replace the editor's. Editor settings are a good place for machine-wide
include directories.

One setting is read only from the editor: `inactive-regions = false` stops
reporting code skipped by the preprocessor (which Helix shows dimmed, but
also lists among the diagnostics).

## Helix

In `~/.config/helix/languages.toml`:

```toml
[language-server.x4c-lsp]
command = "x4c-lsp"
# Optional defaults, merged with each project's x4c-lsp.toml.
config = { include = ["/opt/sde/install/share/p4c/p4include"] }

[[language]]
name = "p4"
scope = "source.p4"
file-types = ["p4"]
comment-token = "//"
block-comment-tokens = { start = "/*", end = "*/" }
language-servers = ["x4c-lsp"]
roots = ["x4c-lsp.toml"]

# Optional syntax highlighting (then run `hx --grammar fetch` and
# `hx --grammar build`).
[[grammar]]
name = "p4"
source = { git = "https://github.com/oxidecomputer/tree-sitter-p4", rev = "cdd541ba9227cf785149b877eb449f2d7ab45033" }
```

Helix 25.07 has no built-in P4 language, so the `[[language]]` entry is
required. Check the setup with `hx --health p4`. Changes to unopened files
and to `x4c-lsp.toml` are picked up when Helix writes them (Helix reports its
own writes to the server; it does not watch the file system).
