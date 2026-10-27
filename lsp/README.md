# x4c-lsp

A language server for P4-16. It gives any editor that speaks the Language
Server Protocol diagnostics, navigation, completion, inlay hints, and code
actions for P4 programs. It works with programs written for any target
(v1model, PSA, Tofino's TNA/T2NA, x4c's SoftNPU), not only those compiled
with `x4c`, and it is developed and tested with Helix 25.07.

- [Quick start](#quick-start)
- [How programs are found](#how-programs-are-found)
- [Configuration](#configuration-x4c-lsptoml)
- [Switching configurations from the editor](#switching-configurations-from-the-editor)
- [Features](#features)
- [Command-line checks](#command-line-checks)
- [Troubleshooting](#troubleshooting)

## Quick start

### 1. Install

From a checkout of this repository:

```sh
cargo install --path lsp --locked
```

Or without cloning (your default Rust toolchain must be 1.92 or newer):

```sh
cargo install --git https://github.com/oxidecomputer/p4 x4c-lsp --locked
```

Either installs the `x4c-lsp` binary into `~/.cargo/bin`. Check it with
`x4c-lsp --version`.

### 2. Set up Helix

Helix 25.07 has no built-in P4 language, so add one to
`~/.config/helix/languages.toml`:

```toml
[language-server.x4c-lsp]
command = "x4c-lsp"

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

To see inlay hints (parameter names and closing labels), also set this in
`~/.config/helix/config.toml`:

```toml
[editor.lsp]
display-inlay-hints = true
```

Run `hx --health p4` to check the setup, then open a `.p4` file.

Other editors: run `x4c-lsp` with no arguments as the server for files with
the `.p4` extension (language ID `p4`). It speaks LSP over stdin and stdout
and logs to stderr.

### 3. Point it at your headers (if needed)

Programs whose includes are next to them or in a standard p4c installation
need no configuration: see
[without a configuration file](#without-a-configuration-file) for the
directories searched by default. Otherwise, such as for a Tofino program
built against the SDE, add an `x4c-lsp.toml` at the root of your project
([configuration](#configuration-x4c-lsptoml)).

## How programs are found

P4 programs are analyzed as a whole, starting from a *root* file (the one
that instantiates `main`) and following its `#include`s. This is what makes
fragment files work: a file such as dendrite's `route_selector.p4`, which is
included inside a control body, is analyzed in the context of the program
that includes it.

Without configuration, the root is inferred from the opened file's
directory. A project with several programs, or one built with different
`-D` defines, lists its roots in `x4c-lsp.toml`.

The analysis is independent of x4c's code generator and of any target.
Architecture headers such as `core.p4`, `v1model.p4`, `tna.p4`, and
`softnpu.p4` are ordinary includes. For x4c's own dialect, the names x4c
declares implicitly (`NoAction`, the `exact`/`ternary`/`lpm`/`range` match
kinds, and the `SoftNPU` package) resolve when a program does not declare
them itself.

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
  closure contains the opened file is used. A file outside the project
  (such as an architecture header reached with go to definition) uses the
  program that includes it; any other file is analyzed on its own.

### Editor settings

The same schema can be passed as JSON in the client's initialization options
or `workspace/didChangeConfiguration` settings. In Helix, put it under
`config` in the language server entry:

```toml
[language-server.x4c-lsp]
command = "x4c-lsp"
config = { include = ["/opt/sde/install/share/p4c/p4include"] }
```

Relative paths there are resolved against the workspace root. A project's
`x4c-lsp.toml` takes precedence: its `include`s come first, its `define`s
win, and its roots replace the editor's. Editor settings are a good place
for machine-wide include directories.

One setting is read only from the editor: `inactive-regions = false` stops
reporting code skipped by the preprocessor (which Helix shows dimmed, but
also lists among the diagnostics). Unused-declaration hints are not
affected.

## Switching configurations from the editor

These commands change what the current file is analyzed with, for the rest
of the session, without editing `x4c-lsp.toml` (Helix:
`:lsp-workspace-command`, which lists them):

- `x4c.configure` opens a menu to define or undefine the macros the
  program tests in `#if`/`#ifdef` (and those already defined), or to use
  another root that includes the file, such as the second root in the
  example above.
- `x4c.toggleDefine` toggles one macro, e.g.
  `:lsp-workspace-command x4c.toggleDefine "MULTICAST"` (bind it to a key
  for quick switching). `"NAME=VALUE"` sets a value.
- `x4c.showConfiguration` shows the file's root and defines.
- `x4c.resetConfiguration` returns to the configuration.

Session defines apply to every root on top of the configured ones.

`x4c.configure` and `x4c.showConfiguration` act on the file passed as their
argument (a path or `file://` URI), or else on the P4 file last opened,
edited, or queried. A Steel-enabled Helix can wrap the menu in a command
for the current buffer:

```scheme
(define (lsp-configure)
  (send-lsp-command "x4c-lsp" "workspace/executeCommand"
    (hash "command" "x4c.configure"
          "arguments" (list (editor-document->path
                              (editor->doc-id (editor-focus)))))
    (lambda (_) void)))
```

## Features

Helix key bindings are in parentheses.

### Diagnostics

Reported while typing:

- Preprocessor: missing includes, `#error`, unbalanced conditionals.
- Syntax errors, with recovery so that the rest of the file is still
  analyzed.
- Names: unknown names, states, match kinds, and struct/header members;
  duplicate declarations; actions a table uses as its default or in
  entries but does not list in `actions`.
- Types:
  - fixed-width values of a different width or signedness than expected,
    in assignments, initializers, arguments, and operands;
  - non-`bool` conditions;
  - assignments to constants and to `in` or directionless parameters, and
    unassignable `out`/`inout` arguments;
  - wrong argument counts;
  - integer literals too wide for their `bit<W>` (a warning);
  - `select` cases and table `entries` whose values do not match the keys'
    types or count;
  - assignments between different structs and headers;
  - `extract`/`emit` of values that are not headers.

  Types that are generic or not fully known are never reported. x4c
  converts between widths implicitly, so for programs using its `SoftNPU`
  package width mismatches are warnings.
- Faded "unnecessary" hints: code skipped by `#if`/`#ifdef`, and unused
  local declarations in controls, parsers, actions, and functions.

### Navigation

- Go to definition (`gd`), declaration (`gD`), and type definition (`gy`),
  including through `#include` paths and macro names. A fragment included
  in several places offers each declaration it resolves to.
- Go to implementation (`gi`): from an architecture's parser, control, or
  package type (such as `tna.p4`'s `control Ingress`), or a package
  parameter, to the blocks and instances the program passes for it; from
  an action to the tables that list it; and from a package or extern type
  to its instances.
- Find references (`gr`), document highlight (with writes marked), and
  rename (`space r`).
- Document symbols (`space s`, nested) and workspace symbols (`space S`).

### Hover (`space k`)

The declaration, its container, and its doc comment (the comment above
it, or the one trailing it on the same line). In addition:

- headers show their size and each field's bit offsets, and header fields
  their position;
- constants show their value, and integer literals their value in decimal,
  hex, and binary;
- macro uses show what they expand to.

### Completion and signature help

- Members after `.`, and names in scope, with keywords and macros.
- Auto-import: names declared in headers the program does not include yet
  are offered too, and accepting one adds the `#include`. Headers in the
  directories the program already includes from come first, so a Tofino
  program is offered Tofino headers.
- Actions, functions, and methods complete as calls with parameter
  placeholders (except where a table or `switch` names an action).
- Snippets for `table`, `parser`, `control`, `state`, `transition select`,
  and other skeletons.
- In `#include` lines, paths complete from the include directories.
- Signature help for actions, functions, extern methods and constructors,
  and parser/control `apply`, listing every overload.

### Inlay hints

Parameter names before call arguments (unless the argument already says
it), and labels after the closing `}` of long declarations and after
`#else`/`#endif` of long conditionals. Helix shows them only with
`display-inlay-hints = true` (see the [quick start](#2-set-up-helix)).

### Code actions (`space a`)

- Quick fixes: change a misspelled name, member, state, or match kind to a
  similar one in scope; create a missing action (parameters guessed from
  the call) or parser state; add the `#include` for a header that declares
  an unknown name; cast a value to the expected width; insert a missing
  `;`; add an action to a table's `actions`; remove an unused declaration.
- Generate: the missing cases of a `switch (t.apply().action_run)`; `emit`
  calls in a deparser for each header field; parser states for headers
  that are never extracted.
- Rewrite: replace an integer literal with an existing constant of the
  same value; inline a macro use.

## Command-line checks

`x4c-lsp check [-I DIR]... [-D NAME[=VALUE]]... FILE...` analyzes each
FILE as a root, prints its diagnostics, and exits with status 1 if there are
errors, which is handy in CI:

```sh
x4c-lsp check -I dpd/p4 -I $SDE/install/share/p4c/p4include \
    -D __TARGET_TOFINO__=2 dpd/p4/sidecar.p4
```

## Troubleshooting

- `hx --health p4` shows whether Helix finds the language and the
  `x4c-lsp` binary.
- The server logs to stderr; in Helix, open the log with `:log-open`.
- Missing-include errors mean the include directories are not set up: add
  an `x4c-lsp.toml` (or `config` in the editor) with the right `include`
  list.
- `x4c.showConfiguration` shows which root and defines the current file is
  analyzed with.
- Changes to unopened files and to `x4c-lsp.toml` are picked up when Helix
  writes them (Helix reports its own writes to the server; it does not
  watch the file system).
