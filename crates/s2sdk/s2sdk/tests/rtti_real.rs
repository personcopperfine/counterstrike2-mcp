//! The RTTI identity gate over the **real** game binaries: every vtable pin of
//! `crates/cs2-bridge/src/generated/pins.rs` resolved through its own RTTI on the staged
//! Windows DLLs, plus the Linux half over the `.so`'s exported `_ZTV` symbols.
//!
//! Like `crates/cs2-bridge/tests/modules_real.rs` this needs files that are not in git (Valve's
//! binaries under `target/game-bins`, gitignored), so it is its own test target with
//! `test = false` in `Cargo.toml`:
//!
//! ```text
//! cargo test -p s2sdk --test rtti_real -- --nocapture
//! ```
//!
//! A missing tree or a missing module fails loudly, naming what is missing - this test never
//! skips itself. `CS2_BINS` / `CS2_BINS_LINUX` point the two halves elsewhere.
//!
//! # File offsets versus the loader's layout
//!
//! The Windows half maps each DLL the way the loader does (`map_image`, copied from
//! `cs2-bridge/src/pe.rs:215`'s `pe::map_image`: headers at RVA 0, each section at its
//! `VirtualAddress`, gaps zero) and uses the file's `ImageBase` as the base, which is what the
//! file's own absolute pointers were written for. The RTTI records the reader walks are RVAs
//! inside that image and the `pSelf` field is an RVA too, so these checks are the same checks
//! a live load would pass.
//!
//! The Linux half maps `.so` the same way from its section table and applies `.rela.dyn`
//! (`R_X86_64_RELATIVE`, `R_X86_64_64`, `R_X86_64_GLOB_DAT`) at base 0, i.e. it builds the
//! image the dynamic linker would have produced had it loaded the module at address 0. Only
//! the pointer slots matter to the reader, and both of those are relocations of exactly those
//! three types.
//!
//! # Where the expectations come from
//!
//! The pins (module, RVA) are *read*, never copied: the generated table is `#[path]`-included
//! below, so this test cannot drift from `pins.py`. The expected class names are the sigs
//! database's own `locator.class`/`via` for each entry (`sigs/signatures/<module>.json`),
//! hand-checked there; each assertion below names the entry it comes from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use s2sdk::rtti::{class_name, is_class, is_class_at, ImageRange, RttiError};

/// The generated pin table, included rather than copied: same rows `pins.py` emits for
/// `bridge_pins.h`.
#[path = "../../../cs2-bridge/src/generated/pins.rs"]
#[allow(dead_code)]
mod pins;

// ---------------------------------------------------------------------------
// The pins under test, by const name (the numbers stay in the generated table)
// ---------------------------------------------------------------------------

/// Every `Pin` of `kind: "vtable"` in the generated table, in table order. The names are the
/// generated consts' names, so a row cannot be missed without this list failing to compile.
const VTABLE_PINS: &[(&str, &pins::Pin)] = &[
    ("HV_HAMMERAPP_VT", &pins::HV_HAMMERAPP_VT),
    ("HV_SESSION_VT", &pins::HV_SESSION_VT),
    ("HV_DOCSESSION_VT", &pins::HV_DOCSESSION_VT),
    ("HV_DOC_VT", &pins::HV_DOC_VT),
    ("HV_ROOT_VT", &pins::HV_ROOT_VT),
    ("HV_HISTORY_VT", &pins::HV_HISTORY_VT),
    ("HV_SELECTION_VT", &pins::HV_SELECTION_VT),
    ("HV_DATAMODEL_VT", &pins::HV_DATAMODEL_VT),
    ("HV_MAPVIEW_VT", &pins::HV_MAPVIEW_VT),
    ("HV_MAPVIEWMGR_VT", &pins::HV_MAPVIEWMGR_VT),
    ("HV_MAPMESH_VT", &pins::HV_MAPMESH_VT),
    ("HV_MAPENTITY_VT", &pins::HV_MAPENTITY_VT),
    ("HV_MAPGROUP_VT", &pins::HV_MAPGROUP_VT),
    ("HV_MAPWORLD_VT", &pins::HV_MAPWORLD_VT),
    ("HV_MAPPREFAB_VT", &pins::HV_MAPPREFAB_VT),
    ("HV_MAPINSTANCE_VT", &pins::HV_MAPINSTANCE_VT),
    ("TFV_FRAMEWORK_VT", &pins::TFV_FRAMEWORK_VT),
    ("SV_SCENESYSTEM_VT", &pins::SV_SCENESYSTEM_VT),
    ("SV_SCENEUTILS_VT", &pins::SV_SCENEUTILS_VT),
    ("SV_SCENEVIEW_VT", &pins::SV_SCENEVIEW_VT),
    ("SV_PIPELINE_ALIAS_VT", &pins::SV_PIPELINE_ALIAS_VT),
    ("SV_SCENEWORLD_VT", &pins::SV_SCENEWORLD_VT),
    ("EV_RENDERSERVICE_VT", &pins::EV_RENDERSERVICE_VT),
    ("EV_ENGINECLIENT_VT", &pins::EV_ENGINECLIENT_VT),
    ("EV_INPUTSERVICE_VT", &pins::EV_INPUTSERVICE_VT),
    ("RV_RENDERDEVICE_VT", &pins::RV_RENDERDEVICE_VT),
    ("CV_CSGOINPUT_VT", &pins::CV_CSGOINPUT_VT),
    ("GSV_GAMECONFIG_SERVER_VT", &pins::GSV_GAMECONFIG_SERVER_VT),
    ("T0V_TESTSCRIPTMGR_VT", &pins::T0V_TESTSCRIPTMGR_VT),
    ("SSV_SCHEMASYSTEM_VT", &pins::SSV_SCHEMASYSTEM_VT),
    ("SSV_TYPESCOPE_VT", &pins::SSV_TYPESCOPE_VT),
    ("SNV_SOUNDOPSYSTEM_VT", &pins::SNV_SOUNDOPSYSTEM_VT),
    ("SNV_SOUNDEVENTMGR_VT", &pins::SNV_SOUNDEVENTMGR_VT),
    ("MDV_APP_VT", &pins::MDV_APP_VT),
    ("MDV_SESSION_VT", &pins::MDV_SESSION_VT),
    ("MDV_DOC_VT", &pins::MDV_DOC_VT),
    ("MDV_EDITORWND_VT", &pins::MDV_EDITORWND_VT),
    ("MDV_SCENEWIDGET_VT", &pins::MDV_SCENEWIDGET_VT),
    ("MDV_TOOLWIDGET_VT", &pins::MDV_TOOLWIDGET_VT),
    ("MDV_TOOLSCENE_VT", &pins::MDV_TOOLSCENE_VT),
    ("MDV_SCENECAMERA_VT", &pins::MDV_SCENECAMERA_VT),
    ("PETV_APP_VT", &pins::PETV_APP_VT),
    ("PETV_SESSION_VT", &pins::PETV_SESSION_VT),
    ("PETV_DOC_VT", &pins::PETV_DOC_VT),
    ("PETV_ROOTNODE_VT", &pins::PETV_ROOTNODE_VT),
    ("PETV_UNDOHIST_VT", &pins::PETV_UNDOHIST_VT),
    ("PETV_MAINWND_VT", &pins::PETV_MAINWND_VT),
    ("PETV_PREVIEW_VT", &pins::PETV_PREVIEW_VT),
    ("PETV_SCENEWIDGET_VT", &pins::PETV_SCENEWIDGET_VT),
    ("PETV_TOOLSCENE_VT", &pins::PETV_TOOLSCENE_VT),
    ("PETV_SCENECAMERA_VT", &pins::PETV_SCENECAMERA_VT),
];

/// Hand-checked expected simple names, each with the sigs entry it comes from
/// (`sigs/signatures/<module>.json`, `entries.<name>.locator.class`, which is itself derived
/// from the RTTI name in `via`). The table prints all 51 pins; these are the ones asserted.
const EXPECTED: &[(&str, &str)] = &[
    // hammer.json: HV_HAMMERAPP_VT -> CHammerApp, "pins.py VTABLES: .?AVCHammerApp@@ primary vtable"
    ("HV_HAMMERAPP_VT", "CHammerApp"),
    // hammer.json: HV_SESSION_VT -> CHammerEditorSession
    ("HV_SESSION_VT", "CHammerEditorSession"),
    // hammer.json: HV_DOC_VT -> CMapDoc
    ("HV_DOC_VT", "CMapDoc"),
    // hammer.json: HV_ROOT_VT -> CMapRootElement
    ("HV_ROOT_VT", "CMapRootElement"),
    // hammer.json: HV_MAPENTITY_VT -> CMapEntity
    ("HV_MAPENTITY_VT", "CMapEntity"),
    // hammer.json: HV_MAPWORLD_VT -> CMapWorld
    ("HV_MAPWORLD_VT", "CMapWorld"),
    // toolframework2.json: TFV_FRAMEWORK_VT -> CToolFramework2
    ("TFV_FRAMEWORK_VT", "CToolFramework2"),
    // scenesystem.json: SV_SCENEVIEW_VT -> CSceneView
    ("SV_SCENEVIEW_VT", "CSceneView"),
    // scenesystem.json: SV_PIPELINE_ALIAS_VT -> CRenderingPipelineAlias
    ("SV_PIPELINE_ALIAS_VT", "CRenderingPipelineAlias"),
    // engine2.json: EV_ENGINECLIENT_VT -> CEngineClient
    ("EV_ENGINECLIENT_VT", "CEngineClient"),
    // rendersystemvulkan.json: RV_RENDERDEVICE_VT -> CRenderDeviceVulkan
    ("RV_RENDERDEVICE_VT", "CRenderDeviceVulkan"),
    // client.json: CV_CSGOINPUT_VT -> CCSGOInput
    ("CV_CSGOINPUT_VT", "CCSGOInput"),
    // server.json: GSV_GAMECONFIG_SERVER_VT -> CCSGameConfiguration
    ("GSV_GAMECONFIG_SERVER_VT", "CCSGameConfiguration"),
    // tier0.json: T0V_TESTSCRIPTMGR_VT -> CTestScriptMgr
    ("T0V_TESTSCRIPTMGR_VT", "CTestScriptMgr"),
    // schemasystem.json: SSV_SCHEMASYSTEM_VT -> CSchemaSystem
    ("SSV_SCHEMASYSTEM_VT", "CSchemaSystem"),
    // schemasystem.json: SSV_TYPESCOPE_VT -> CSchemaSystemTypeScope
    ("SSV_TYPESCOPE_VT", "CSchemaSystemTypeScope"),
    // soundsystem.json: SNV_SOUNDOPSYSTEM_VT -> CSoundOpSystem
    ("SNV_SOUNDOPSYSTEM_VT", "CSoundOpSystem"),
    // soundsystem.json: SNV_SOUNDEVENTMGR_VT -> CSoundOpSystem too: the entry's own `via` is
    // "ISoundEventManager base vptr of CSoundOpSystem at +0x08", i.e. a secondary vtable of the
    // same class. The pin's *name* says EventMgr; the RTTI says CSoundOpSystem, and so does the
    // sigs entry's `locator.class`.
    ("SNV_SOUNDEVENTMGR_VT", "CSoundOpSystem"),
    // modeldoc_editor.json: MDV_APP_VT -> CModelDocEditorApp
    ("MDV_APP_VT", "CModelDocEditorApp"),
    // modeldoc_editor.json: MDV_DOC_VT -> CModelDoc
    ("MDV_DOC_VT", "CModelDoc"),
    // pet.json: PETV_APP_VT -> CPetApp
    ("PETV_APP_VT", "CPetApp"),
    // pet.json: PETV_UNDOHIST_VT -> CUndoHistory
    ("PETV_UNDOHIST_VT", "CUndoHistory"),
];

// ---------------------------------------------------------------------------
// The Windows bins tree
// ---------------------------------------------------------------------------

/// The pinned modules' file names, found anywhere under the bins tree (the install layout puts
/// public modules under `game/bin/win64`, the client/server ones under `csgo/bin/win64` and the
/// tools' ones under `game/bin/win64/tools`).
struct Bins {
    paths: HashMap<&'static str, PathBuf>,
}

impl Bins {
    fn load(root: &Path) -> Self {
        assert!(
            root.is_dir(),
            "the real-binary gate needs the staged bins tree at {} (sigs/README.md \"Binaries\": \
             python3 sigs/scripts/stagebins.py --game workshop/run/app --out target/game-bins); \
             set CS2_BINS to point elsewhere",
            root.display()
        );
        let mut found: HashMap<String, PathBuf> = HashMap::new();
        walk(root, &mut found);
        let mut missing = Vec::new();
        let mut paths = HashMap::new();
        for module in pins::PIN_MODULES {
            match found.get(&module.name.to_ascii_lowercase()) {
                Some(path) => {
                    paths.insert(module.name, path.clone());
                }
                None => missing.push(module.name),
            }
        }
        assert!(
            missing.is_empty(),
            "the bins tree {} is missing: {}",
            root.display(),
            missing.join(", ")
        );
        Self { paths }
    }

    fn bytes(&self, name: &str) -> Vec<u8> {
        let path = self
            .paths
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not a pinned module"));
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    fn image(&self, name: &str) -> MappedImage {
        let bytes = self.bytes(name);
        map_image(&bytes).unwrap_or_else(|error| panic!("{name} must map as PE32+: {error:?}"))
    }
}

/// Every file under `dir` that is named after a pinned module.
fn walk(dir: &Path, found: &mut HashMap<String, PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("a readable directory entry");
        let path = entry.path();
        let metadata = entry
            .metadata()
            .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
        if metadata.is_dir() {
            walk(&path, found);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        if pins::PIN_MODULES.iter().any(|module| module.name == name) {
            found.insert(name, path);
        }
    }
}

fn bins_root(variable: &str, default: &str) -> PathBuf {
    match std::env::var_os(variable) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(default),
    }
}

// ---------------------------------------------------------------------------
// A PE32+ mapper, copied from `cs2-bridge/src/pe.rs:215` (`pe::map_image`)
// ---------------------------------------------------------------------------

/// A file mapped the way the Windows loader maps an image: headers at RVA 0, sections at their
/// `VirtualAddress`, gaps zero-filled. `bytes.len()` is `SizeOfImage`.
struct MappedImage {
    bytes: Vec<u8>,
    image_base: u64,
}

#[derive(Debug)]
enum MapError {
    NotPe32Plus,
    OutOfRange,
}

fn u16_at(file: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        file.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(file: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        file.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(file: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        file.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// The offsets below are `winnt.h`'s `IMAGE_DOS_HEADER`/`IMAGE_NT_HEADERS64`/
/// `IMAGE_SECTION_HEADER` fields, the same ones `pe.rs` names.
const PE_E_LFANEW_OFF: usize = 0x3c;
const PE_SIG: u32 = 0x0000_4550;
const PE_OPTIONAL_HDR_OFF: usize = 24;
const PE32PLUS_MAGIC: u16 = 0x20b;
const PE32PLUS_IMAGEBASE: usize = 24;
const PE32PLUS_SIZEOFIMAGE: usize = 56;
const PE32PLUS_SIZEOFHEADERS: usize = 60;
const PE_NUMBER_OF_SECTIONS_OFF: usize = 6;
const PE_SIZE_OF_OPTIONAL_HEADER_OFF: usize = 20;
const PE_SECTION_HEADER_BYTES: usize = 40;
const PE_SECTION_VIRTUAL_SIZE_OFF: usize = 8;
const PE_SECTION_VIRTUAL_ADDRESS_OFF: usize = 12;
const PE_SECTION_RAW_SIZE_OFF: usize = 16;
const PE_SECTION_RAW_PTR_OFF: usize = 20;

fn map_image(file: &[u8]) -> Result<MappedImage, MapError> {
    let pe = usize::try_from(u32_at(file, PE_E_LFANEW_OFF).ok_or(MapError::NotPe32Plus)?)
        .map_err(|_| MapError::NotPe32Plus)?;
    if u32_at(file, pe) != Some(PE_SIG) {
        return Err(MapError::NotPe32Plus);
    }
    let opt = pe
        .checked_add(PE_OPTIONAL_HDR_OFF)
        .ok_or(MapError::OutOfRange)?;
    if u16_at(file, opt) != Some(PE32PLUS_MAGIC) {
        return Err(MapError::NotPe32Plus);
    }
    let image_base = u64_at(
        file,
        opt.checked_add(PE32PLUS_IMAGEBASE)
            .ok_or(MapError::OutOfRange)?,
    )
    .ok_or(MapError::OutOfRange)?;
    let size_of_image = usize::try_from(
        u32_at(
            file,
            opt.checked_add(PE32PLUS_SIZEOFIMAGE)
                .ok_or(MapError::OutOfRange)?,
        )
        .ok_or(MapError::OutOfRange)?,
    )
    .map_err(|_| MapError::OutOfRange)?;
    let size_of_headers = usize::try_from(
        u32_at(
            file,
            opt.checked_add(PE32PLUS_SIZEOFHEADERS)
                .ok_or(MapError::OutOfRange)?,
        )
        .ok_or(MapError::OutOfRange)?,
    )
    .map_err(|_| MapError::OutOfRange)?;
    let sections = u16_at(
        file,
        pe.checked_add(PE_NUMBER_OF_SECTIONS_OFF)
            .ok_or(MapError::OutOfRange)?,
    )
    .ok_or(MapError::OutOfRange)?;
    let optional_size = u16_at(
        file,
        pe.checked_add(PE_SIZE_OF_OPTIONAL_HEADER_OFF)
            .ok_or(MapError::OutOfRange)?,
    )
    .ok_or(MapError::OutOfRange)?;
    if size_of_headers > size_of_image {
        return Err(MapError::OutOfRange);
    }
    let mut bytes = vec![0u8; size_of_image];
    let header_end = size_of_headers.min(file.len());
    bytes[..header_end].copy_from_slice(&file[..header_end]);

    let table = opt
        .checked_add(usize::from(optional_size))
        .ok_or(MapError::OutOfRange)?;
    for index in 0..usize::from(sections) {
        let header = table
            .checked_add(
                index
                    .checked_mul(PE_SECTION_HEADER_BYTES)
                    .ok_or(MapError::OutOfRange)?,
            )
            .ok_or(MapError::OutOfRange)?;
        let virtual_size =
            u32_at(file, header + PE_SECTION_VIRTUAL_SIZE_OFF).ok_or(MapError::OutOfRange)?;
        let virtual_address =
            u32_at(file, header + PE_SECTION_VIRTUAL_ADDRESS_OFF).ok_or(MapError::OutOfRange)?;
        let raw_size =
            u32_at(file, header + PE_SECTION_RAW_SIZE_OFF).ok_or(MapError::OutOfRange)?;
        let raw_pointer =
            u32_at(file, header + PE_SECTION_RAW_PTR_OFF).ok_or(MapError::OutOfRange)?;
        let virtual_size = usize::try_from(virtual_size).map_err(|_| MapError::OutOfRange)?;
        let virtual_address = usize::try_from(virtual_address).map_err(|_| MapError::OutOfRange)?;
        let raw_size = usize::try_from(raw_size).map_err(|_| MapError::OutOfRange)?;
        let raw_pointer = usize::try_from(raw_pointer).map_err(|_| MapError::OutOfRange)?;
        let length = if virtual_size == 0 {
            raw_size
        } else {
            virtual_size.min(raw_size)
        };
        if length == 0 {
            continue;
        }
        let source = raw_pointer
            .checked_add(length)
            .ok_or(MapError::OutOfRange)?;
        let destination = virtual_address
            .checked_add(length)
            .ok_or(MapError::OutOfRange)?;
        if source > file.len() || destination > size_of_image {
            return Err(MapError::OutOfRange);
        }
        bytes[virtual_address..destination].copy_from_slice(&file[raw_pointer..source]);
    }
    Ok(MappedImage { bytes, image_base })
}

/// Every vtable pin, through its own RTTI, on the real Windows DLLs.
#[test]
fn every_windows_vtable_pin_resolves_through_rtti() {
    let bins = Bins::load(&bins_root("CS2_BINS", "../../../target/game-bins"));
    let mut images: HashMap<&'static str, MappedImage> = HashMap::new();
    let mut resolved = 0usize;
    println!("pin name | module | RVA | decorated name | simple name | top offset");
    for (name, pin) in VTABLE_PINS {
        assert_eq!(pin.kind, "vtable", "{name} is not a vtable pin");
        let module = pins::PIN_MODULES[usize::from(pin.module)].name;
        let image = images.entry(module).or_insert_with(|| bins.image(module));
        let range = ImageRange::msvc(&image.bytes, image.image_base);
        let vptr = image.image_base + pin.value;
        let class = class_name(vptr, &range)
            .unwrap_or_else(|error| panic!("{name} ({module} RVA {:#x}): {error}", pin.value));
        println!(
            "{name} | {module} | {:#x} | {} | {} | {}",
            pin.value,
            class.decorated(),
            class.simple(),
            class.top_offset()
        );
        resolved += 1;
        // `is_class` is the primary-table question (offset 0); `is_class_at` with the table's
        // own offset must match every pin, primary or secondary.
        assert_eq!(
            is_class(vptr, &range, class.simple()),
            Ok(class.top_offset() == 0),
            "{name}: is_class disagrees with the table's top offset"
        );
        assert_eq!(
            is_class_at(vptr, &range, class.simple(), class.top_offset()),
            Ok(true),
            "{name}: is_class_at disagrees with class_name"
        );
        assert_eq!(
            is_class(vptr, &range, "DefinitelyNotThisClass"),
            Ok(false),
            "{name}: a wrong name matched"
        );
    }
    assert_eq!(resolved, VTABLE_PINS.len(), "every vtable pin was resolved");
    assert!(resolved >= 8, "the gate asserts at least eight pins");

    let mut checked = 0usize;
    for (name, expected) in EXPECTED {
        let (_, pin) = VTABLE_PINS
            .iter()
            .find(|(pin_name, _)| pin_name == name)
            .unwrap_or_else(|| panic!("{name} is not in the pin table"));
        let module = pins::PIN_MODULES[usize::from(pin.module)].name;
        let image = images.get(module).expect("every module was mapped");
        let class = class_name(
            image.image_base + pin.value,
            &ImageRange::msvc(&image.bytes, image.image_base),
        )
        .expect("resolved above");
        assert_eq!(class.simple(), *expected, "{name}");
        checked += 1;
    }
    assert!(checked >= 8, "only {checked} hand-checked pins");

    // The two pins that share a class through a secondary vtable: both name CSoundOpSystem, and
    // the second is the ISoundEventManager base table. `soundsystem.json` measures the locators
    // at 0x4dcc50 (offset 0) and 0x4dcc28 (offset 8) with the same type descriptor 0x553280;
    // the raw locator offset is read back here from the mapped file, and the reader must report
    // it in the offset-to-top convention (0 and -8).
    let sound = images
        .get("soundsystem.dll")
        .expect("soundsystem.dll is mapped");
    let range = ImageRange::msvc(&sound.bytes, sound.image_base);
    let ops_vptr = sound.image_base + pins::SNV_SOUNDOPSYSTEM_VT.value;
    let evt_vptr = sound.image_base + pins::SNV_SOUNDEVENTMGR_VT.value;
    let ops = class_name(ops_vptr, &range).expect("SNV_SOUNDOPSYSTEM_VT resolves");
    let evt = class_name(evt_vptr, &range).expect("SNV_SOUNDEVENTMGR_VT resolves");
    assert_eq!(ops.decorated(), ".?AVCSoundOpSystem@@");
    assert_eq!(evt.decorated(), ".?AVCSoundOpSystem@@");
    let ops_locator_offset = locator_offset(&sound, pins::SNV_SOUNDOPSYSTEM_VT.value);
    let evt_locator_offset = locator_offset(&sound, pins::SNV_SOUNDEVENTMGR_VT.value);
    println!(
        "soundsystem pair: SNV_SOUNDOPSYSTEM_VT locator offset {ops_locator_offset:#x}, \
         top offset {}; SNV_SOUNDEVENTMGR_VT locator offset {evt_locator_offset:#x}, \
         top offset {}",
        ops.top_offset(),
        evt.top_offset()
    );
    assert_eq!(ops_locator_offset, 0, "the primary table's locator offset");
    assert_eq!(evt_locator_offset, 8, "the +8 base table's locator offset");
    assert_eq!(ops.top_offset(), 0);
    assert_eq!(evt.top_offset(), -8);
    // Only the primary table answers `is_class`; the +8 table is the same class at -8.
    assert_eq!(is_class(ops_vptr, &range, "CSoundOpSystem"), Ok(true));
    assert_eq!(is_class(evt_vptr, &range, "CSoundOpSystem"), Ok(false));
    assert_eq!(
        is_class_at(evt_vptr, &range, "CSoundOpSystem", -8),
        Ok(true)
    );

    println!("{resolved} vtable pins resolved, {checked} hand-checked");
}

/// The raw `RTTICompleteObjectLocator::offset` of the table whose vptr is at `vptr_rva`, read
/// straight from the mapped image (the hand measurement the reader's `top_offset` is held to).
fn locator_offset(image: &MappedImage, vptr_rva: u64) -> u32 {
    let slot = usize::try_from(vptr_rva - 8).expect("an RVA");
    let pointer = u64::from_le_bytes(
        image.bytes[slot..slot + 8]
            .try_into()
            .expect("vptr[-1] is a word in the image"),
    );
    let locator = usize::try_from(pointer - image.image_base).expect("an RVA in the image");
    u32::from_le_bytes(
        image.bytes[locator + 4..locator + 8]
            .try_into()
            .expect("the locator is 24 bytes"),
    )
}

// ---------------------------------------------------------------------------
// The Linux bins tree: exported `_ZTV` symbols
// ---------------------------------------------------------------------------

/// An ELF image built from the section table at base 0, with `.rela.dyn` applied - the image
/// the dynamic linker would have produced if it had loaded the module at address 0.
struct ElfImage {
    bytes: Vec<u8>,
    /// `(name, st_value, st_shndx)` of every dynamic symbol.
    symbols: Vec<(String, u64, u16)>,
}

/// `Elf64_Shdr`/`Elf64_Sym`/`Elf64_Rela` fields and the relocation types the pointer slots
/// use (`elf.h`).
const ELF_SH_TYPE: usize = 4;
const ELF_SH_FLAGS: usize = 8;
const ELF_SH_ADDR: usize = 16;
const ELF_SH_OFFSET: usize = 24;
const ELF_SH_SIZE: usize = 32;
const ELF_SH_LINK: usize = 40;
const SHF_ALLOC: u64 = 2;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;
const SHT_DYNSYM: u32 = 11;
const R_X86_64_64: u64 = 1;
const R_X86_64_GLOB_DAT: u64 = 6;
const R_X86_64_RELATIVE: u64 = 8;
const SYM_BYTES: usize = 24;
const RELA_BYTES: usize = 24;

#[derive(Clone, Copy)]
struct Section {
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
}

fn load_elf(path: &Path) -> ElfImage {
    let file =
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    assert!(
        file.starts_with(b"\x7fELF"),
        "{} is not an ELF image",
        path.display()
    );
    let shoff = u64_at(&file, 0x28).expect("e_shoff");
    let shentsize = usize::from(u16_at(&file, 0x3a).expect("e_shentsize"));
    let shnum = usize::from(u16_at(&file, 0x3c).expect("e_shnum"));
    let sections: Vec<Section> = (0..shnum)
        .map(|index| {
            let at = usize::try_from(shoff).expect("e_shoff fits") + index * shentsize;
            Section {
                kind: u32_at(&file, at + ELF_SH_TYPE).expect("sh_type"),
                flags: u64_at(&file, at + ELF_SH_FLAGS).expect("sh_flags"),
                addr: u64_at(&file, at + ELF_SH_ADDR).expect("sh_addr"),
                offset: u64_at(&file, at + ELF_SH_OFFSET).expect("sh_offset"),
                size: u64_at(&file, at + ELF_SH_SIZE).expect("sh_size"),
                link: u32_at(&file, at + ELF_SH_LINK).expect("sh_link"),
            }
        })
        .collect();

    // The image is the span of the allocated sections; each one is copied to its `sh_addr`.
    let span = sections
        .iter()
        .filter(|section| section.flags & SHF_ALLOC != 0)
        .map(|section| section.addr + section.size)
        .max()
        .unwrap_or(0);
    let mut bytes = vec![0u8; usize::try_from(span).expect("a file-sized image")];
    for section in &sections {
        if section.flags & SHF_ALLOC == 0 || section.kind == SHT_NOBITS || section.size == 0 {
            continue;
        }
        let start = usize::try_from(section.addr).expect("an address in the image");
        let len = usize::try_from(section.size).expect("a file-sized section");
        let at = usize::try_from(section.offset).expect("a file offset");
        bytes[start..start + len].copy_from_slice(&file[at..at + len]);
    }

    let dynsym = sections
        .iter()
        .find(|section| section.kind == SHT_DYNSYM)
        .unwrap_or_else(|| panic!("{} has no .dynsym", path.display()));
    let dynstr = sections[usize::try_from(dynsym.link).expect("a section index")];
    let strtab = usize::try_from(dynstr.offset).expect("a file offset");
    let count = usize::try_from(dynsym.size).expect("a file size") / SYM_BYTES;
    let mut symbols = Vec::with_capacity(count);
    for index in 0..count {
        let at = usize::try_from(dynsym.offset).expect("a file offset") + index * SYM_BYTES;
        let name = u32_at(&file, at).expect("st_name");
        let start = strtab + usize::try_from(name).expect("a string offset");
        let end = file[start..]
            .iter()
            .position(|&byte| byte == 0)
            .map(|len| start + len)
            .expect("a NUL-terminated dynamic symbol name");
        let value = u64_at(&file, at + 8).expect("st_value");
        let section = u16_at(&file, at + 6).expect("st_shndx");
        symbols.push((
            String::from_utf8_lossy(&file[start..end]).into_owned(),
            value,
            section,
        ));
    }

    // `.rela.dyn` at base 0: the three types a pointer slot can be.
    let mut applied = 0usize;
    for section in &sections {
        if section.kind != SHT_RELA {
            continue;
        }
        let count = usize::try_from(section.size).expect("a file size") / RELA_BYTES;
        for index in 0..count {
            let at = usize::try_from(section.offset).expect("a file offset") + index * RELA_BYTES;
            let offset = u64_at(&file, at).expect("r_offset");
            let info = u64_at(&file, at + 8).expect("r_info");
            let addend = u64_at(&file, at + 16).expect("r_addend") as i64;
            let kind = info & 0xffff_ffff;
            let symbol = usize::try_from(info >> 32).expect("a symbol index");
            let value = match kind {
                R_X86_64_RELATIVE => u64::try_from(addend).ok(),
                R_X86_64_64 => symbols
                    .get(symbol)
                    .map(|(_, value, _)| value.wrapping_add_signed(addend)),
                R_X86_64_GLOB_DAT => symbols.get(symbol).map(|(_, value, _)| *value),
                _ => None,
            };
            let Some(value) = value else { continue };
            let Some(start) = usize::try_from(offset).ok() else {
                continue;
            };
            if let Some(slot) = bytes.get_mut(start..start + 8) {
                slot.copy_from_slice(&value.to_le_bytes());
                applied += 1;
            }
        }
    }
    println!(
        "{}: image {:#x} bytes, {} dynamic symbols, {applied} relocations",
        path.file_name().unwrap_or_default().to_string_lossy(),
        bytes.len(),
        symbols.len()
    );
    ElfImage { bytes, symbols }
}

/// Every exported `_ZTV` symbol of one module, through the Itanium reader. Returns
/// `(resolved, skipped)` where a skipped symbol is one the reader refused, with the error: on
/// these modules the only legitimate refusal is a type_info pointer of 0, an `_ZTI` imported
/// from another module (`type_info` for `double`, `std::string`'s
/// `basic_string<char, char_traits<char>, allocator<char>>`, ...), which this image cannot
/// read - the reader fails closed instead of following a pointer into another mapping.
fn check_exported_vtables(
    path: &Path,
    expected: &[(&str, &str)],
) -> (usize, Vec<(String, RttiError)>) {
    let elf = load_elf(path);
    let range = ImageRange::itanium(&elf.bytes, 0);
    let mut resolved = 0usize;
    let mut skipped = Vec::new();
    let mut mismatched = Vec::new();
    for (name, value, section) in &elf.symbols {
        let Some(mangled) = name.strip_prefix("_ZTV") else {
            continue;
        };
        if *section == 0 {
            continue; // undefined
        }
        // The `_ZTV` symbol labels the vtable group: offset-to-top, type_info, then the first
        // function - the object's vptr points at the third word (Itanium ABI 2.5.2).
        let vptr = value + 16;
        match class_name(vptr, &range) {
            Ok(class) => {
                resolved += 1;
                if class.decorated() != mangled {
                    mismatched.push(format!("{name}: {} != {mangled}", class.decorated()));
                }
                for (wanted, simple) in expected {
                    if name == wanted {
                        assert_eq!(class.simple(), *simple, "{name}");
                        println!(
                            "{name}: decorated {} simple {}",
                            class.decorated(),
                            class.simple()
                        );
                    }
                }
            }
            Err(error) => skipped.push((name.clone(), error)),
        }
    }
    assert!(
        mismatched.is_empty(),
        "exported _ZTV symbols whose RTTI name is not the symbol's: {mismatched:?}"
    );
    for (wanted, _) in expected {
        assert!(
            elf.symbols.iter().any(|(name, _, _)| name == wanted),
            "{wanted} is not exported by {}",
            path.display()
        );
    }
    for (name, error) in &skipped {
        assert!(
            matches!(
                error,
                RttiError::OutsideAddress {
                    address: 0,
                    what: "the Itanium type_info",
                    ..
                }
            ),
            "{name}: the only expected refusal is an imported type_info at address 0, got {error:?}"
        );
    }
    (resolved, skipped)
}

#[test]
fn linux_exported_vtables_resolve_through_itanium_rtti() {
    let root = bins_root("CS2_BINS_LINUX", "../../../target/game-bins-linux-2000930");
    assert!(
        root.is_dir(),
        "the Linux half needs the staged tree at {} (sigs/README.md \"Binaries\"); set \
         CS2_BINS_LINUX to point elsewhere",
        root.display()
    );
    let mut found = HashMap::new();
    walk_any(&root, &mut found);
    let tier0 = found
        .get("libtier0.so")
        .unwrap_or_else(|| panic!("{} has no libtier0.so", root.display()))
        .clone();

    // The two sigs entries that name an Itanium vtable by its exported `_ZTV` symbol
    // (`sigs/signatures/libtier0.json`: `_ZTV27IKeyValuesDumpContextAsText`,
    // `_ZTV29CKeyValuesDumpContextAsDevMsg`).
    let (resolved, skipped) = check_exported_vtables(
        &tier0,
        &[
            (
                "_ZTV27IKeyValuesDumpContextAsText",
                "IKeyValuesDumpContextAsText",
            ),
            (
                "_ZTV29CKeyValuesDumpContextAsDevMsg",
                "CKeyValuesDumpContextAsDevMsg",
            ),
            ("_ZTV7CThread", "CThread"),
        ],
    );
    assert!(
        resolved >= 20,
        "only {resolved} of libtier0's exported _ZTV symbols resolved"
    );
    println!(
        "libtier0.so: {resolved} resolved, {} not in the image",
        skipped.len()
    );

    // The big module: no game class is exported (its `_ZTV` set is the statically linked C++
    // runtime), but the reader still has to agree with every one of them.
    let client = found
        .get("libclient.so")
        .unwrap_or_else(|| panic!("{} has no libclient.so", root.display()))
        .clone();
    let (resolved, skipped) = check_exported_vtables(&client, &[]);
    assert!(
        resolved >= 150,
        "only {resolved} of libclient's exported _ZTV symbols resolved"
    );
    println!(
        "libclient.so: {resolved} resolved, {} not in the image (imported type_info)",
        skipped.len()
    );
    for (name, error) in skipped.iter().take(5) {
        println!("  skipped: {name}: {error}");
    }
}

/// Every file under `dir` by file name.
fn walk_any(dir: &Path, found: &mut HashMap<String, PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("a readable directory entry");
        let path = entry.path();
        if path.is_dir() {
            walk_any(&path, found);
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            found.insert(name.to_owned(), path);
        }
    }
}
