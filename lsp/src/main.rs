// Copyright 2026 Oxide Computer Company

use std::path::PathBuf;
use x4c_lsp::analysis::Analysis;
use x4c_lsp::config::{normalize_path, parse_define};
use x4c_lsp::diag::Severity;
use x4c_lsp::preprocess::Options;
use x4c_lsp::source::{PositionEncoding, SourceDb};

const USAGE: &str = "\
usage: x4c-lsp [--version | --help]
       x4c-lsp check [-I DIR]... [-D NAME[=VALUE]]... FILE...

A P4-16 language server. Without arguments, speaks the Language Server
Protocol on stdin and stdout and logs to stderr. See lsp/README.md for
configuration.

`check` analyzes each FILE as a compilation root and prints its
diagnostics, exiting with status 1 if there are errors.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--stdio") => run(),
        Some("--version" | "-V") => {
            println!("x4c-lsp {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help" | "-h") => println!("{USAGE}"),
        Some("check") => check(&args[1..]),
        Some(other) => usage_error(&format!("unknown argument `{other}`")),
    }
}

fn usage_error(msg: &str) -> ! {
    eprintln!("x4c-lsp: {msg}\n{USAGE}");
    std::process::exit(2);
}

fn run() {
    if let Err(e) = x4c_lsp::server::run() {
        eprintln!("x4c-lsp: {e}");
        std::process::exit(1);
    }
}

fn check(args: &[String]) {
    let mut opts = Options::default();
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> String {
            match arg.strip_prefix(flag).filter(|v| !v.is_empty()) {
                Some(v) => v.to_string(),
                None => it.next().cloned().unwrap_or_else(|| {
                    usage_error(&format!("{flag} needs a value"))
                }),
            }
        };
        if arg.starts_with("-I") {
            opts.include_dirs
                .push(normalize_path(&PathBuf::from(value("-I"))));
        } else if arg.starts_with("-D") {
            match parse_define(&value("-D")) {
                Ok(d) => opts.defines.push(d),
                Err(e) => usage_error(&e),
            }
        } else if arg.starts_with('-') {
            usage_error(&format!("unknown option `{arg}`"));
        } else {
            files.push(normalize_path(&PathBuf::from(arg)));
        }
    }
    if files.is_empty() {
        usage_error("check needs at least one FILE");
    }
    let mut errors = 0;
    for file in files {
        let mut db = SourceDb::default();
        let Some(root) = db.load(&file) else {
            eprintln!("x4c-lsp: cannot read {}", file.display());
            errors += 1;
            continue;
        };
        let a = Analysis::new(db, root, &opts);
        for d in a.diagnostics() {
            let severity = match d.severity {
                Severity::Error => {
                    errors += 1;
                    "error"
                }
                Severity::Warning => "warning",
                // Hints mark inactive preprocessor blocks; editors fade
                // them, and they are noise on a terminal.
                Severity::Info | Severity::Hint => continue,
            };
            let pos = a.db.line_col(
                d.span.file,
                d.span.start,
                PositionEncoding::Utf32,
            );
            println!(
                "{}:{}:{}: {severity}: {}",
                a.db.path(d.span.file).display(),
                pos.line + 1,
                pos.col + 1,
                d.message
            );
        }
    }
    if errors > 0 {
        std::process::exit(1);
    }
}
