//! The export gate on synthesized images: minimal PE32+ and ELF64 files built here, byte by byte,
//! so the tests need no toolchain, no build artefact and no Valve binary. Each image carries
//! exactly the directories the gate reads (export, import, delay-import; `.dynsym` + `.dynstr`),
//! which is also what a real linker writes, so a parse the gate gets right here is the parse it
//! runs on the staged files (`cross_check_with_llvm_tools` compares the two on real artefacts).

use std::path::{Path, PathBuf};
use std::process::Command;

use cs2_bridge_tools::exports::{check_bytes, elf, is_crt_import, pe, Format, Gate};

// ---------------------------------------------------------------- PE32+

/// A PE32+ DLL with one `.rdata` section holding every directory.
#[derive(Default)]
struct Pe {
    names: Vec<&'static str>,
    unnamed: usize,
    imports: Vec<&'static str>,
    delay_imports: Vec<&'static str>,
}

const SECTION_RVA: u32 = 0x1000;
const SECTION_FILE: usize = 0x200;

impl Pe {
    fn exports(names: &[&'static str]) -> Self {
        Pe { names: names.to_vec(), imports: vec!["KERNEL32.dll", "ntdll.dll"], ..Pe::default() }
    }

    fn build(&self) -> Vec<u8> {
        // Section data first; `put` returns the RVA of what it placed.
        let mut data: Vec<u8> = Vec::new();
        let mut put = |bytes: &[u8]| -> u32 {
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
            let rva = SECTION_RVA + data.len() as u32;
            data.extend_from_slice(bytes);
            rva
        };
        let cstr = |s: &str| [s.as_bytes(), b"\0"].concat();

        let mut export_dir = None;
        if !self.names.is_empty() || self.unnamed > 0 {
            let dll_name = put(&cstr("synth.dll"));
            let functions = self.names.len() + self.unnamed;
            // Every entry points at the section start: what it points at does not matter.
            let eat = put(&SECTION_RVA.to_le_bytes().repeat(functions));
            let name_rvas: Vec<u32> = self.names.iter().map(|n| put(&cstr(n))).collect();
            let name_table = put(&name_rvas.iter().flat_map(|r| r.to_le_bytes()).collect::<Vec<_>>());
            // Named entries first, then the ordinal-only ones.
            let ordinals = put(&(0..self.names.len() as u16).flat_map(u16::to_le_bytes).collect::<Vec<_>>());
            let mut dir = Vec::new();
            for v in [0u32, 0, 0, dll_name, 1, functions as u32, self.names.len() as u32, eat, name_table, ordinals] {
                dir.extend_from_slice(&v.to_le_bytes());
            }
            // Characteristics, TimeDateStamp, Major+Minor (one u32: both zero), Name, Base, ...
            export_dir = Some((put(&dir), 40u32));
        }

        let mut import_dir = None;
        if !self.imports.is_empty() {
            let names: Vec<u32> = self.imports.iter().map(|n| put(&cstr(n))).collect();
            let mut table = Vec::new();
            for name in names {
                for v in [0u32, 0, 0, name, 0] {
                    table.extend_from_slice(&v.to_le_bytes());
                }
            }
            table.extend_from_slice(&[0; 20]);
            let len = table.len() as u32;
            import_dir = Some((put(&table), len));
        }

        let mut delay_dir = None;
        if !self.delay_imports.is_empty() {
            let names: Vec<u32> = self.delay_imports.iter().map(|n| put(&cstr(n))).collect();
            let mut table = Vec::new();
            for name in names {
                for v in [1u32, name, 0, 0, 0, 0, 0, 0] {
                    table.extend_from_slice(&v.to_le_bytes());
                }
            }
            table.extend_from_slice(&[0; 32]);
            let len = table.len() as u32;
            delay_dir = Some((put(&table), len));
        }

        let raw_size = data.len().next_multiple_of(0x200);
        let mut file = vec![0u8; SECTION_FILE + raw_size];
        file[..2].copy_from_slice(b"MZ");
        file[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        let pe = 0x40;
        file[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let w16 = |f: &mut Vec<u8>, off: usize, v: u16| f[off..off + 2].copy_from_slice(&v.to_le_bytes());
        let w32 = |f: &mut Vec<u8>, off: usize, v: u32| f[off..off + 4].copy_from_slice(&v.to_le_bytes());
        w16(&mut file, pe + 4, 0x8664); // Machine: AMD64
        w16(&mut file, pe + 6, 1); // NumberOfSections
        w16(&mut file, pe + 20, 240); // SizeOfOptionalHeader (PE32+, 16 directories)
        w16(&mut file, pe + 22, 0x2022); // Characteristics: DLL | LARGE_ADDRESS_AWARE | EXECUTABLE
        let opt = pe + 24;
        w16(&mut file, opt, 0x20B);
        file[opt + 24..opt + 32].copy_from_slice(&0x1_8000_0000u64.to_le_bytes()); // ImageBase
        w32(&mut file, opt + 32, 0x1000); // SectionAlignment
        w32(&mut file, opt + 36, 0x200); // FileAlignment
        w32(&mut file, opt + 56, SECTION_RVA + (data.len() as u32).next_multiple_of(0x1000)); // SizeOfImage
        w32(&mut file, opt + 60, SECTION_FILE as u32); // SizeOfHeaders
        w32(&mut file, opt + 108, 16); // NumberOfRvaAndSizes
        for (index, dir) in [(0, export_dir), (1, import_dir), (13, delay_dir)] {
            if let Some((rva, size)) = dir {
                w32(&mut file, opt + 112 + 8 * index, rva);
                w32(&mut file, opt + 112 + 8 * index + 4, size);
            }
        }
        let sh = opt + 240;
        file[sh..sh + 6].copy_from_slice(b".rdata");
        w32(&mut file, sh + 8, data.len() as u32); // VirtualSize
        w32(&mut file, sh + 12, SECTION_RVA); // VirtualAddress
        w32(&mut file, sh + 16, raw_size as u32); // SizeOfRawData
        w32(&mut file, sh + 20, SECTION_FILE as u32); // PointerToRawData
        w32(&mut file, sh + 36, 0x4000_0040); // INITIALIZED_DATA | MEM_READ
        file[SECTION_FILE..SECTION_FILE + data.len()].copy_from_slice(&data);
        file
    }
}

fn pe_verdict(image: &Pe, gate: Gate) -> Vec<String> {
    check_bytes(&image.build(), Format::Pe, gate).expect("the synthesized PE parses").problems
}

#[test]
fn pe_reader_reads_back_what_was_written() {
    let image = Pe { unnamed: 1, delay_imports: vec!["dbghelp.dll"], ..Pe::exports(&["CreateInterface", "Other"]) };
    let facts = pe::read(&image.build()).unwrap();
    assert_eq!(facts.export_entries, 3);
    assert_eq!(facts.named_entries, 2);
    assert_eq!(facts.export_names, ["CreateInterface", "Other"]);
    assert_eq!(facts.imports, ["KERNEL32.dll", "ntdll.dll", "dbghelp.dll"]);
}

#[test]
fn pe_bridge_with_exactly_create_interface_passes() {
    let v = check_bytes(&Pe::exports(&["CreateInterface"]).build(), Format::Pe, Gate::Bridge).unwrap();
    assert!(v.passed(), "{:?}", v.problems);
    assert_eq!(v.names, ["CreateInterface"]);
}

#[test]
fn pe_extra_export_is_refused() {
    let problems = pe_verdict(&Pe::exports(&["CreateInterface", "rust_eh_personality"]), Gate::Bridge);
    assert_eq!(problems, ["UNEXPECTED export 'rust_eh_personality'"]);
}

#[test]
fn pe_ordinal_only_export_is_refused() {
    let problems = pe_verdict(&Pe { unnamed: 1, ..Pe::exports(&["CreateInterface"]) }, Gate::Bridge);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("2 export entries but 1 named ones"), "{problems:?}");
}

#[test]
fn pe_crt_imports_are_refused_in_both_import_directories() {
    let image = Pe {
        imports: vec!["KERNEL32.dll", "VCRUNTIME140.dll", "api-ms-win-crt-runtime-l1-1-0.dll"],
        delay_imports: vec!["ucrtbase.dll"],
        ..Pe::exports(&["CreateInterface"])
    };
    let problems = pe_verdict(&image, Gate::Bridge);
    assert_eq!(problems.len(), 3, "{problems:?}");
    for dll in ["VCRUNTIME140.dll", "api-ms-win-crt-runtime-l1-1-0.dll", "ucrtbase.dll"] {
        assert!(problems.iter().any(|p| p.contains(&format!("imports '{dll}'")) && p.contains("not crt-static")), "{dll}: {problems:?}");
    }
}

#[test]
fn crt_names_match_in_any_case_and_nothing_else_does() {
    for dll in ["vcruntime140_1.dll", "MSVCP140.dll", "msvcp140.dll", "API-MS-WIN-CRT-HEAP-L1-1-0.DLL", "UCRTBASE.DLL", "concrt140.dll", "CONCRT140.dll"] {
        assert!(is_crt_import(dll), "{dll}");
    }
    for dll in ["KERNEL32.dll", "ntdll.dll", "api-ms-win-core-synch-l1-2-0.dll", "msvcrt.dll", "bcryptprimitives.dll"] {
        assert!(!is_crt_import(dll), "{dll}");
    }
}

#[test]
fn pe_missing_create_interface_is_refused() {
    assert_eq!(pe_verdict(&Pe::exports(&[]), Gate::Bridge), ["MISSING CreateInterface"]);
    assert_eq!(
        pe_verdict(&Pe::exports(&["CreateInterfaceX"]), Gate::Bridge),
        ["MISSING CreateInterface", "UNEXPECTED export 'CreateInterfaceX'"]
    );
}

#[test]
fn pe_loader_gate_wants_exactly_dllmain() {
    assert!(pe_verdict(&Pe::exports(&["DllMain"]), Gate::Loader).is_empty());
    assert_eq!(
        pe_verdict(&Pe::exports(&["CreateInterface"]), Gate::Loader),
        ["MISSING DllMain", "UNEXPECTED export 'CreateInterface'"]
    );
    let problems = pe_verdict(&Pe { imports: vec!["VCRUNTIME140.dll"], ..Pe::exports(&["DllMain"]) }, Gate::Loader);
    assert_eq!(problems.len(), 1, "{problems:?}");
}

#[test]
fn malformed_pe_cannot_be_checked() {
    let good = Pe::exports(&["CreateInterface"]).build();
    assert!(check_bytes(&good[..0x100], Format::Pe, Gate::Bridge).is_err(), "truncated before the section table");
    let mut no_sig = good.clone();
    no_sig[0x40] = b'X';
    assert!(check_bytes(&no_sig, Format::Pe, Gate::Bridge).is_err());
    // The export directory pointing outside every section.
    let mut wild = good.clone();
    let export_rva = 0x40 + 24 + 112;
    wild[export_rva..export_rva + 4].copy_from_slice(&0x0090_0000u32.to_le_bytes());
    assert!(check_bytes(&wild, Format::Pe, Gate::Bridge).is_err());
    assert!(check_bytes(b"\x7fELF", Format::Pe, Gate::Bridge).is_err());
}

// ---------------------------------------------------------------- ELF64

/// An ELF64 `.so` with `.dynstr` and `.dynsym`: `(name, defined)` symbols after the null one.
fn elf(symbols: &[(&str, bool)]) -> Vec<u8> {
    let mut dynstr = vec![0u8];
    let mut dynsym = vec![0u8; 24]; // the null symbol
    for (name, defined) in symbols {
        let off = dynstr.len() as u32;
        dynstr.extend_from_slice(name.as_bytes());
        dynstr.push(0);
        let mut sym = [0u8; 24];
        sym[..4].copy_from_slice(&off.to_le_bytes());
        sym[4] = 0x12; // STB_GLOBAL | STT_FUNC
        let shndx: u16 = if *defined { 2 } else { 0 };
        sym[6..8].copy_from_slice(&shndx.to_le_bytes());
        dynsym.extend_from_slice(&sym);
    }
    let dynstr_off = 64usize;
    let dynsym_off = (dynstr_off + dynstr.len()).next_multiple_of(8);
    let shoff = (dynsym_off + dynsym.len()).next_multiple_of(8);
    let mut file = vec![0u8; shoff + 3 * 64];
    file[..4].copy_from_slice(b"\x7fELF");
    file[4] = 2; // ELFCLASS64
    file[5] = 1; // ELFDATA2LSB
    file[6] = 1; // EV_CURRENT
    file[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    file[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    file[20..24].copy_from_slice(&1u32.to_le_bytes());
    file[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
    file[0x34..0x36].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    file[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    file[0x3C..0x3E].copy_from_slice(&3u16.to_le_bytes()); // e_shnum
    file[dynstr_off..dynstr_off + dynstr.len()].copy_from_slice(&dynstr);
    file[dynsym_off..dynsym_off + dynsym.len()].copy_from_slice(&dynsym);
    let section = |file: &mut Vec<u8>, i: usize, kind: u32, off: usize, size: usize, link: u32, entsize: u64| {
        let s = shoff + 64 * i;
        file[s + 4..s + 8].copy_from_slice(&kind.to_le_bytes());
        file[s + 24..s + 32].copy_from_slice(&(off as u64).to_le_bytes());
        file[s + 32..s + 40].copy_from_slice(&(size as u64).to_le_bytes());
        file[s + 40..s + 44].copy_from_slice(&link.to_le_bytes());
        file[s + 56..s + 64].copy_from_slice(&entsize.to_le_bytes());
    };
    section(&mut file, 1, 3, dynstr_off, dynstr.len(), 0, 0); // SHT_STRTAB
    section(&mut file, 2, 11, dynsym_off, dynsym.len(), 1, 24); // SHT_DYNSYM -> .dynstr
    file
}

/// The undefined imports a real cdylib carries next to its one definition.
const IMPORTS: [(&str, bool); 3] = [("malloc", false), ("__cxa_finalize", false), ("_Unwind_Resume", false)];

fn elf_verdict(defined: &[&str], gate: Gate) -> Vec<String> {
    let mut symbols = IMPORTS.to_vec();
    symbols.extend(defined.iter().map(|n| (*n, true)));
    check_bytes(&elf(&symbols), Format::Elf, gate).expect("the synthesized ELF parses").problems
}

#[test]
fn elf_reader_lists_only_defined_symbols() {
    let image = elf(&[("malloc", false), ("CreateInterface", true), ("free", false), ("helper", true)]);
    assert_eq!(elf::defined_dynamic_symbols(&image).unwrap(), ["CreateInterface", "helper"]);
}

#[test]
fn elf_bridge_with_exactly_create_interface_passes() {
    assert!(elf_verdict(&["CreateInterface"], Gate::Bridge).is_empty());
}

#[test]
fn elf_extra_export_is_refused() {
    assert_eq!(elf_verdict(&["CreateInterface", "_fini"], Gate::Bridge), ["UNEXPECTED export '_fini'"]);
}

#[test]
fn elf_missing_create_interface_is_refused() {
    assert_eq!(elf_verdict(&[], Gate::Bridge), ["MISSING CreateInterface"]);
}

#[test]
fn elf_loader_gate_wants_exactly_dlopen() {
    assert!(elf_verdict(&["dlopen"], Gate::Loader).is_empty());
    assert_eq!(elf_verdict(&["dlopen", "dlsym"], Gate::Loader), ["UNEXPECTED export 'dlsym'"]);
    assert_eq!(elf_verdict(&["CreateInterface"], Gate::Loader), ["MISSING dlopen", "UNEXPECTED export 'CreateInterface'"]);
}

#[test]
fn malformed_elf_cannot_be_checked() {
    let good = elf(&[("CreateInterface", true)]);
    assert!(check_bytes(&good[..good.len() - 1], Format::Elf, Gate::Bridge).is_err(), "truncated section headers");
    let mut no_dynsym = good.clone();
    let shoff = u64::from_le_bytes(good[0x28..0x30].try_into().unwrap()) as usize;
    no_dynsym[shoff + 128 + 4] = 2; // SHT_SYMTAB instead of SHT_DYNSYM
    assert!(check_bytes(&no_dynsym, Format::Elf, Gate::Bridge).is_err());
    let mut elf32 = good.clone();
    elf32[4] = 1;
    assert!(check_bytes(&elf32, Format::Elf, Gate::Bridge).is_err());
    assert!(check_bytes(&Pe::exports(&["CreateInterface"]).build(), Format::Elf, Gate::Bridge).is_err());
}

// ---------------------------------------------------------------- the command

fn check_exports(args: &[&str], file: &Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cs2-bridge-tools"))
        .arg("check-exports")
        .args(args)
        .arg(file)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap(), text)
}

#[test]
fn the_command_exits_0_1_or_2() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let write = |name: &str, bytes: Vec<u8>| -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    };

    let ok = write("cs2_bridge.dll", Pe::exports(&["CreateInterface"]).build());
    let (code, text) = check_exports(&[], &ok);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("exports exactly CreateInterface"), "{text}");

    let (code, text) = check_exports(&["--loader"], &ok);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("MISSING DllMain") && text.contains("FAILED for"), "{text}");

    let so = write("libcs2mcp_mmload.so", elf(&[("dlopen", true), ("dlsym", false)]));
    let (code, text) = check_exports(&["--loader"], &so);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("exports exactly dlopen"), "{text}");

    let (code, text) = check_exports(&[], &dir.path().join("missing.dll"));
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("no such file"), "{text}");

    let exe = write("cs2mcp_launch.exe", Pe::exports(&["CreateInterface"]).build());
    let (code, text) = check_exports(&[], &exe);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("neither a .dll nor a .so"), "{text}");

    let garbage = write("garbage.so", b"not an elf".to_vec());
    let (code, text) = check_exports(&[], &garbage);
    assert_eq!(code, 2, "{text}");
}

// ---------------------------------------------------------------- real artefacts

/// The native reader against llvm-readobj / llvm-nm on the staged build artefacts: the same
/// export names, the same entry count, the same import DLLs, the same defined dynamic symbols.
///
/// Needs `cargo bridge build` (and `cargo bridge testplugin` for the fixture's pair, checked
/// when staged) and `llvm-readobj` + `llvm-nm` on PATH. Run with
/// `cargo test -p cs2-bridge-tools --test exports -- --ignored`.
#[test]
#[ignore = "needs the staged artefacts of `cargo bridge build` and the llvm tools"]
fn cross_check_with_llvm_tools() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files: Vec<PathBuf> = [
        "target/bridge-rs/win64/cs2_bridge.dll",
        "target/bridge-rs/win64/cs2mcp_mmload.dll",
        "target/bridge-rs/linuxsteamrt64/libcs2_bridge.so",
        "target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so",
    ]
    .iter()
    .map(|f| root.join(f))
    .collect();
    for f in &files {
        assert!(f.is_file(), "{} is not staged: run `cargo bridge build` first", f.display());
    }
    for f in ["target/testplugin/win64/mm_testplugin.dll", "target/testplugin/linuxsteamrt64/libmm_testplugin.so"] {
        if root.join(f).is_file() {
            files.push(root.join(f));
        }
    }
    let tool = |name: &str, args: &[&str], file: &Path| -> String {
        let out = Command::new(name)
            .args(args)
            .arg(file)
            .output()
            .unwrap_or_else(|e| panic!("{name} is needed for this test: {e}"));
        assert!(out.status.success(), "{name} {args:?} {}: {}", file.display(), String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let field = |dump: &str, key: &str| -> Vec<String> {
        dump.lines()
            .filter_map(|l| l.trim_start().strip_prefix(key))
            .map(str::to_owned)
            .collect()
    };
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        match Format::of(file).unwrap() {
            Format::Pe => {
                let facts = pe::read(&bytes).unwrap();
                let exports = tool("llvm-readobj", &["--coff-exports"], file);
                let mut names = field(&exports, "Name: ");
                names.retain(|n| !n.is_empty());
                let mut ours = facts.export_names.clone();
                ours.sort();
                names.sort();
                assert_eq!(ours, names, "{}", file.display());
                assert_eq!(facts.export_entries, field(&exports, "Ordinal: ").len(), "{}", file.display());
                let imports = tool("llvm-readobj", &["--coff-imports"], file);
                assert_eq!(facts.imports, field(&imports, "Name: "), "{}", file.display());
            }
            Format::Elf => {
                let nm = tool("llvm-nm", &["-D", "--defined-only"], file);
                let mut theirs: Vec<String> = nm
                    .lines()
                    .filter_map(|l| {
                        let cols: Vec<_> = l.split_whitespace().collect();
                        (cols.len() >= 3).then(|| cols[cols.len() - 1].to_owned())
                    })
                    .collect();
                let mut ours = elf::defined_dynamic_symbols(&bytes).unwrap();
                theirs.sort();
                ours.sort();
                assert_eq!(ours, theirs, "{}", file.display());
            }
        }
    }
}
