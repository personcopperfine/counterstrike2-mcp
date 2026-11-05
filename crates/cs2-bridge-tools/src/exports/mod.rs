//! `cargo bridge check-exports [--loader] <file.dll|file.so>`: the export gate on a built
//! artefact, read natively from the PE export/import directories and the ELF `.dynsym` (no
//! llvm-readobj / llvm-nm on the host).
//!
//! # The bridge gate (the default): exactly `CreateInterface`
//!
//! The Rust cdylib is a drop-in for the C bridge's `mcp_bridge.dll`, whose only exported symbol
//! was `CreateInterface` (`mcp_bridge.c:6846`: the single `__declspec(dllexport)` in that file).
//! One extra export is a leak of Rust internals into the game's symbol space and, for a tool
//! plugin, an invitation for the engine to call something that was never meant to be an entry
//! point - so this is a hard gate, not a warning. The same gate holds the coexistence fixture
//! (`crates/mm-testplugin`), whose one export is `CreateInterface` too.
//!
//! Nothing is allow-listed beyond it, on purpose. PE: the cdylib's export table is exactly what
//! `#[no_mangle]` asks for (one entry). ELF: the linker adds no symbol of its own for this
//! toolchain - the staged `.so`'s dynamic symbols are undefined libc/libgcc imports plus exactly
//! one defined `CreateInterface`. If a future toolchain adds a linker-generated dynamic symbol
//! (`_init`/`_fini`/`__bss_start`/`_edata`/`_end` are the usual candidates) the gate fails, and
//! the name must be justified here before it is allowed: the point is that the export set is
//! exactly `CreateInterface`, not that it is close to it.
//!
//! # The loader gate (`--loader`): one symbol, the one the loader needs
//!
//! * `cs2mcp_mmload.dll` (Wine): exactly `{DllMain}`. The loader is not a plugin, and `DllMain`
//!   is not an API either: the PE loader reaches it through the image's entry point
//!   (`_DllMainCRTStartup` -> `DllMain`), the game never looks it up. It is in the export table
//!   because rustc's cdylib export list holds every `pub` `#[no_mangle]` item and `DllMain` is
//!   one.
//! * `libcs2mcp_mmload.so` (Linux): exactly `{dlopen}`. Every other `dlopen` in the process goes
//!   through the interposer, so an extra definition (a Rust helper leaked into the dynamic symbol
//!   table, a second interposer symbol) would be new behaviour in the dynamic loader's namespace.
//!
//! # Both gates, on a DLL
//!
//! * An ordinal-only export cannot be reviewed, so the export address table must have a name for
//!   every entry (the entry count and the named-entry count must agree).
//! * No CRT import (`VCRUNTIME*`, `MSVCP*`, `api-ms-win-crt-*`, `ucrtbase*`, `CONCRT*`, any case;
//!   import and delay-import directories): the DLL is loaded under Wine, whose prefix does not
//!   necessarily carry `vcruntime140.dll`. `+crt-static` (`.cargo/config.toml`) is what keeps
//!   those imports out, and an environment `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` would silently
//!   replace that config while the link still succeeds - so the artefact is checked, not the
//!   intent.
//!
//! The format is chosen by the extension (`.dll` / `.so`), as the shell gates did; the content
//! must then parse as that format.
//!
//! Exit codes: `0` the gate passed, `1` it refused the file, `2` the file could not be checked
//! (missing, neither `.dll` nor `.so`, or not a well-formed image of that format).

pub mod elf;
pub mod pe;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The command line of `cargo bridge check-exports`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Apply the Metamod loader's gate (`DllMain` / `dlopen`) instead of the bridge's
    /// (`CreateInterface`)
    #[arg(long)]
    pub loader: bool,
    /// The artefact: a `.dll` (PE) or a `.so` (ELF)
    pub file: PathBuf,
}

/// Runs the command; `Ok` is the exit code.
pub fn run(args: Args) -> Result<u8> {
    let gate = if args.loader { Gate::Loader } else { Gate::Bridge };
    Ok(gate_file(&args.file, gate))
}

/// Which export set a file is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// A Metamod/tool plugin (`cs2_bridge`, `mm_testplugin`): exactly `CreateInterface`.
    Bridge,
    /// The Metamod loader (`cs2mcp_mmload`): `DllMain` on Windows, `dlopen` on Linux.
    Loader,
}

/// The two artefact formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// A Windows `.dll` (PE32+).
    Pe,
    /// A Linux `.so` (ELF64).
    Elf,
}

impl Format {
    /// The format named by `path`'s extension, `None` for anything but `.dll` / `.so`.
    pub fn of(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()? {
            "dll" => Some(Format::Pe),
            "so" => Some(Format::Elf),
            _ => None,
        }
    }
}

impl Gate {
    /// The exact export set of `format` under this gate.
    pub fn expected(self, format: Format) -> &'static [&'static str] {
        match (self, format) {
            (Gate::Bridge, _) => &["CreateInterface"],
            (Gate::Loader, Format::Pe) => &["DllMain"],
            (Gate::Loader, Format::Elf) => &["dlopen"],
        }
    }
}

/// What the gate found in one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The exact export set the file is held to.
    pub expected: &'static [&'static str],
    /// The exported names (PE: the export name table; ELF: the defined dynamic symbols),
    /// sorted and de-duplicated.
    pub names: Vec<String>,
    /// Every rule the file breaks, one message each; empty when the gate passes.
    pub problems: Vec<String>,
}

impl Verdict {
    /// Whether the file passed.
    pub fn passed(&self) -> bool {
        self.problems.is_empty()
    }
}

/// The DLL name prefixes of the CRTs a crt-static image must not import (compared in lower
/// case: Windows DLL names are case-insensitive).
const CRT_PREFIXES: [&str; 5] = ["vcruntime", "msvcp", "api-ms-win-crt-", "ucrtbase", "concrt"];

/// Whether `dll` is a CRT DLL [`CRT_PREFIXES`] refuses.
pub fn is_crt_import(dll: &str) -> bool {
    let lower = dll.to_ascii_lowercase();
    CRT_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Gates the image `bytes` of `format`; `Err` when the bytes are not a well-formed image of that
/// format (the file cannot be checked).
pub fn check_bytes(bytes: &[u8], format: Format, gate: Gate) -> Result<Verdict> {
    let expected = gate.expected(format);
    let mut problems = Vec::new();
    let mut names = match format {
        Format::Pe => {
            let facts = pe::read(bytes)?;
            if facts.export_entries != facts.named_entries {
                problems.push(format!(
                    "{} export entries but {} named ones: an unnamed (ordinal-only) export cannot be \
                     reviewed and is refused",
                    facts.export_entries, facts.named_entries
                ));
            }
            for dll in facts.imports.iter().filter(|d| is_crt_import(d)) {
                problems.push(format!(
                    "imports '{dll}': the DLL is not crt-static (-Ctarget-feature=+crt-static is missing \
                     from the rustflags; see .cargo/config.toml)"
                ));
            }
            facts.export_names
        }
        Format::Elf => elf::defined_dynamic_symbols(bytes)?,
    };
    names.sort();
    names.dedup();
    for want in expected {
        if !names.iter().any(|n| n == want) {
            problems.push(format!("MISSING {want}"));
        }
    }
    for name in names.iter().filter(|n| !expected.contains(&n.as_str())) {
        problems.push(format!("UNEXPECTED export '{name}'"));
    }
    Ok(Verdict { expected, names, problems })
}

/// Reads and gates `path` (its format from the extension); `Err` when it cannot be checked.
pub fn check_file(path: &Path, gate: Gate) -> Result<Verdict> {
    if !path.is_file() {
        bail!("no such file: {}", path.display());
    }
    let Some(format) = Format::of(path) else {
        bail!("{} is neither a .dll nor a .so", path.display());
    };
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    check_bytes(&bytes, format, gate).with_context(|| format!("{} cannot be checked", path.display()))
}

/// Gates `path` and reports on stdout/stderr the way the shell gates did; returns the exit code
/// (`0` passed, `1` refused, `2` cannot be checked).
pub fn gate_file(path: &Path, gate: Gate) -> u8 {
    let verdict = match check_file(path, gate) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("check-exports: {e:#}");
            return 2;
        }
    };
    let file = path.display();
    println!("check-exports: {file} exports {} name(s):", verdict.names.len());
    if verdict.names.is_empty() {
        println!("  <none>");
    }
    for name in &verdict.names {
        println!("  {name}");
    }
    for problem in &verdict.problems {
        eprintln!("check-exports: {file}: {problem}");
    }
    if verdict.passed() {
        println!("check-exports: OK - {file} exports exactly {}", verdict.expected.join(" "));
        0
    } else {
        eprintln!("check-exports: FAILED for {file}");
        1
    }
}
