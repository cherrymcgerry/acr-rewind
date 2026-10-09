//! `import-dumper7`: reads a Dumper-7 CppSDK (Basic.hpp + *_classes.hpp), writes the offsets
//! into signatures.toml (comments preserved) and lists vehicle/sim/online class candidates.

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Value};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SdkMember {
    pub name: String,
    pub ty: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SdkClass {
    /// "Class", "ScriptStruct" or "" (predefined / unknown).
    pub kind: String,
    /// `Package.Name` from the `// Class Package.Name` comment.
    pub full_name: String,
    pub cpp_name: String,
    pub super_cpp: Option<String>,
    pub size: u64,
    pub members: Vec<SdkMember>,
    pub functions: Vec<String>,
    pub file: String,
}

impl SdkClass {
    pub fn package(&self) -> &str {
        self.full_name.split_once('.').map_or("", |(p, _)| p)
    }
    /// UE (reflection) name: `Package.Name` → `Name`, else the C++ name minus its U/A/F prefix.
    pub fn ue_name(&self) -> String {
        if let Some((_, n)) = self.full_name.split_once('.') {
            return n.to_string();
        }
        let c = &self.cpp_name;
        match c.chars().next() {
            Some('U' | 'A' | 'F') if c.len() > 1 && c.as_bytes()[1].is_ascii_uppercase() => c[1..].to_string(),
            _ => c.clone(),
        }
    }
    pub fn member(&self, name: &str) -> Option<&SdkMember> {
        self.members.iter().find(|m| m.name == name)
    }
}

fn parse_int(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// `namespace Offsets { constexpr int32 GObjects = 0x...; ... }` → name → value.
pub fn parse_offsets(text: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Some(start) = text.find("namespace Offsets") else {
        return out;
    };
    let body = &text[start..];
    let body = &body[..body.find('}').unwrap_or(body.len())];
    for line in body.lines() {
        if let Some(v) = parse_constexpr(line) {
            out.insert(v.0, v.1);
        }
    }
    out
}

/// `[static] constexpr <type> Name = value;` → (Name, value).
fn parse_constexpr(line: &str) -> Option<(String, u64)> {
    let l = line.trim();
    let rest = l.strip_prefix("static ").unwrap_or(l).strip_prefix("constexpr ")?;
    let (lhs, rhs) = rest.split_once('=')?;
    let name = lhs.split_whitespace().last()?.to_string();
    let value = parse_int(rhs.split(';').next()?)?;
    Some((name, value))
}

/// First `constexpr ... Name = value;` anywhere in `text`.
pub fn find_constant(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .filter(|l| l.contains(name) && l.contains("constexpr"))
        .filter_map(parse_constexpr)
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

/// `class|struct|union [alignas(..)] Name [final] [: public Super]` → (name, super).
fn parse_decl(line: &str) -> Option<(String, Option<String>)> {
    if line.starts_with([' ', '\t']) || line.trim_end().ends_with(';') {
        return None;
    }
    let mut toks = line.split_whitespace().peekable();
    if !matches!(toks.next()?, "class" | "struct" | "union") {
        return None;
    }
    while toks
        .peek()
        .is_some_and(|t| t.starts_with("alignas(") || t.starts_with("SDK_ALIGN") || t.starts_with("__declspec"))
    {
        toks.next();
    }
    let name = toks.next()?.to_string();
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut sup = None;
    while let Some(t) = toks.next() {
        match t {
            "final" => {}
            ":" => {
                let t = toks.next()?;
                let t = if matches!(t, "public" | "private" | "protected") { toks.next()? } else { t };
                sup = Some(t.trim_end_matches(',').to_string());
                break;
            }
            "{" => break,
            _ => return None,
        }
    }
    Some((name, sup))
}

/// `\t<type> <name>[\[N\]][ : bits]; // 0xOFF(0xSIZE)(...)`.
fn parse_member(line: &str) -> Option<SdkMember> {
    let (decl, comment) = line.split_once("//")?;
    let decl = decl.trim().strip_suffix(';')?.trim();
    let comment = comment.trim();
    let (off, rest) = comment.split_once('(')?;
    let offset = parse_int(off)?;
    let size = parse_int(rest.split(')').next()?)?;
    let decl = decl.split(" : ").next()?.trim();
    let (ty, name) = decl.rsplit_once(char::is_whitespace)?;
    let name = name.split('[').next()?.trim_start_matches(['*', '&']);
    Some(SdkMember { name: name.to_string(), ty: ty.trim().to_string(), offset, size })
}

/// Name of a function declared on `line` (inside a class body), if any.
fn parse_function(line: &str) -> Option<String> {
    let l = line.trim();
    if l.starts_with("//") || l.contains("// 0x") || l.starts_with('{') || l.starts_with('}') {
        return None;
    }
    let paren = l.find('(')?;
    let head = l[..paren].trim_end();
    let name = head.rsplit(|c: char| c.is_whitespace() || c == '*' || c == '&').next()?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || head == name {
        return None;
    }
    Some(name.to_string())
}

/// Parses every class/struct body in `text`.
pub fn parse_classes(text: &str, file: &str) -> Vec<SdkClass> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((cpp_name, super_cpp)) = parse_decl(lines[i]) else {
            i += 1;
            continue;
        };
        let mut c = SdkClass { cpp_name, super_cpp, file: file.to_string(), ..Default::default() };
        for back in lines[i.saturating_sub(4)..i].iter().rev() {
            let b = back.trim();
            if let Some(n) = b.strip_prefix("// Class ") {
                c.kind = "Class".into();
                c.full_name = n.trim().to_string();
            } else if let Some(n) = b.strip_prefix("// ScriptStruct ") {
                c.kind = "ScriptStruct".into();
                c.full_name = n.trim().to_string();
            } else if let Some(sz) = b.strip_prefix("// 0x") {
                if let Some(v) = sz.split_whitespace().next().and_then(|h| u64::from_str_radix(h, 16).ok()) {
                    c.size = v;
                }
            } else if !(b.starts_with("#pragma") || b.starts_with("template") || b.starts_with("//")) {
                break;
            }
        }
        i += 1;
        let mut depth = 0i32;
        while i < lines.len() {
            let l = lines[i];
            if l.starts_with("};") {
                break;
            }
            let opens = l.matches('{').count() as i32;
            let closes = l.matches('}').count() as i32;
            if depth == 1 {
                if let Some(m) = parse_member(l) {
                    c.members.push(m);
                } else if let Some(f) = parse_function(l) {
                    if !c.functions.contains(&f) {
                        c.functions.push(f);
                    }
                }
            }
            depth += opens - closes;
            i += 1;
        }
        out.push(c);
        i += 1;
    }
    out
}

// ---- mapping into signatures.toml --------------------------------------------------------

/// Dumper-7 `Offsets::` name → `[raw_offsets]` key. `GNames` is deliberately absent: it is
/// Dumper-7's fallback and need not be the FNamePool (on build 25170642 it was not; Dumper-7
/// itself resolves names through AppendString). See [`gnames_note`].
const OFFSET_KEYS: &[(&str, &str)] = &[
    ("GObjects", "gobjects_rva"),
    ("GWorld", "gworld_rva"),
    ("ProcessEvent", "process_event_rva"),
    ("ProcessEventIdx", "process_event_index"),
    ("AppendString", "append_string_rva"),
];

/// (Dumper-7 class, member) → `[raw_offsets.uobject]` key.
const UOBJECT_KEYS: &[(&str, &str, &str)] = &[
    ("UObject", "VTable", "vtable"),
    ("UObject", "Flags", "object_flags"),
    ("UObject", "Index", "internal_index"),
    ("UObject", "Class", "class"),
    ("UObject", "Name", "name"),
    ("UObject", "Outer", "outer"),
    ("UField", "Next", "ufield_next"),
    ("UStruct", "SuperStruct", "ustruct_super"),
    ("UStruct", "Children", "ustruct_children"),
    ("UStruct", "ChildProperties", "ustruct_child_properties"),
    ("UStruct", "Size", "ustruct_properties_size"),
    ("UFunction", "FunctionFlags", "ufunction_flags"),
    ("UFunction", "ExecFunction", "ufunction_func"),
    ("FField", "ClassPrivate", "ffield_class"),
    ("FField", "Next", "ffield_next"),
    ("FField", "Name", "ffield_name"),
    ("FProperty", "ElementSize", "fproperty_element_size"),
    ("FProperty", "PropertyFlags", "fproperty_flags"),
    ("FProperty", "Offset", "fproperty_offset"),
];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Change {
    pub key: String,
    pub old: Option<String>,
    pub new: String,
    pub source: String,
}

/// Warning about Dumper-7's `GNames` (never imported): compares it with the configured
/// `gnames_rva`, which is kept.
pub fn gnames_note(offsets: &BTreeMap<String, u64>, configured: Option<u64>) -> Option<String> {
    let d7 = *offsets.get("GNames").filter(|v| **v != 0)?;
    Some(match configured {
        Some(c) if c == d7 => format!(
            "WARNING: raw_offsets.gnames_rva {c:#x} equals Dumper-7's GNames, which is only Dumper-7's fallback and \
             was NOT the FNamePool on build 25170642; confirm with `acr-probe sig-test` (the hook falls back to the \
             .data scan if it does not validate)"
        ),
        Some(c) => format!(
            "WARNING: Dumper-7 GNames {d7:#x} differs from raw_offsets.gnames_rva {c:#x}; keeping {c:#x} (GNames is \
             Dumper-7's fallback, not necessarily the FNamePool). Re-check with `acr-probe sig-test`"
        ),
        None => format!(
            "WARNING: Dumper-7 GNames {d7:#x} NOT imported into raw_offsets.gnames_rva (it need not be the FNamePool); \
             the FNamePool is found by the dumper7 / .data scans. Run `acr-probe sig-test` and pin the validated address"
        ),
    })
}

/// Values to write: (table path, key, value, source description).
pub fn planned_values(
    offsets: &BTreeMap<String, u64>,
    constants: &BTreeMap<String, u64>,
    classes: &HashMap<String, &SdkClass>,
) -> Vec<(Vec<&'static str>, String, u64, String)> {
    let mut out = Vec::new();
    for (d7, key) in OFFSET_KEYS {
        if let Some(&v) = offsets.get(*d7) {
            if v != 0 {
                out.push((vec!["raw_offsets"], key.to_string(), v, format!("Offsets::{d7}")));
            }
        }
    }
    if let Some(&v) = constants.get("FNameBlockOffsetBits") {
        out.push((
            vec!["fnamepool", "layout"],
            "block_offset_bits".into(),
            v,
            "FNamePool::FNameBlockOffsetBits".into(),
        ));
    }
    if let Some(&v) = constants.get("FNameEntryStride") {
        out.push((vec!["fnamepool", "layout"], "entry_stride".into(), v, "FNamePool::FNameEntryStride".into()));
    }
    for (cls, member, key) in UOBJECT_KEYS {
        if let Some(m) = classes.get(*cls).and_then(|c| c.member(member)) {
            out.push((vec!["raw_offsets", "uobject"], key.to_string(), m.offset, format!("{cls}::{member}")));
        }
    }
    out
}

fn table_mut<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut toml_edit::Table> {
    let mut t = doc.as_table_mut();
    for p in path {
        let item = t.entry(p).or_insert_with(toml_edit::table);
        t = item.as_table_mut().with_context(|| format!("[{p}] is not a table"))?;
    }
    Ok(t)
}

fn hex_value(v: u64, decimal: bool) -> Value {
    let s = if decimal { v.to_string() } else { format!("{v:#x}") };
    s.parse().expect("integer literal")
}

/// Writes `planned` into the TOML text, preserving comments; returns the new text and changes.
pub fn apply_values(text: &str, planned: &[(Vec<&'static str>, String, u64, String)]) -> Result<(String, Vec<Change>)> {
    let mut doc: DocumentMut = text.parse().context("parsing signatures.toml")?;
    let mut changes = Vec::new();
    for (path, key, v, source) in planned {
        let t = table_mut(&mut doc, path)?;
        let decimal = key == "process_event_index" || key == "block_offset_bits";
        let old = t.get(key).and_then(Item::as_value).cloned();
        if old.as_ref().and_then(Value::as_integer) == Some(*v as i64) {
            continue;
        }
        let mut nv = hex_value(*v, decimal);
        match &old {
            Some(o) => *nv.decor_mut() = o.decor().clone(),
            None => nv.decor_mut().set_suffix(format!(" # Dumper-7 {source}")),
        }
        changes.push(Change {
            key: format!("{}.{key}", path.join(".")),
            old: old.map(|o| o.to_string().trim().to_string()),
            new: nv.to_string().trim().to_string(),
            source: source.clone(),
        });
        t.insert(key, Item::Value(nv));
    }
    Ok((doc.to_string(), changes))
}

/// Prepends class names to a string array (skipping ones already present).
pub fn prepend_classes(text: &str, path: &[&str], key: &str, names: &[String]) -> Result<(String, Vec<Change>)> {
    let mut doc: DocumentMut = text.parse()?;
    let t = table_mut(&mut doc, path)?;
    let arr = t
        .entry(key)
        .or_insert_with(|| Item::Value(Value::Array(Default::default())))
        .as_array_mut()
        .with_context(|| format!("{key} is not an array"))?;
    let mut changes = Vec::new();
    for n in names.iter().rev() {
        if arr.iter().any(|v| v.as_str() == Some(n)) {
            continue;
        }
        let mut v = Value::from(n.as_str());
        v.decor_mut().set_prefix("\n  ");
        arr.insert(0, v);
        changes.push(Change {
            key: format!("{}.{key}", path.join(".")),
            old: None,
            new: n.clone(),
            source: "SDK class candidate".into(),
        });
    }
    Ok((doc.to_string(), changes))
}

// ---- SDK search -------------------------------------------------------------------------

const ENGINE_PACKAGES: &[&str] = &[
    "CoreUObject",
    "Engine",
    "UMG",
    "Slate",
    "SlateCore",
    "InputCore",
    "EnhancedInput",
    "AIModule",
    "NavigationSystem",
    "GameplayTags",
    "GameplayTasks",
    "PhysicsCore",
    "Chaos",
    "ChaosVehicles",
    "ChaosVehiclesCore",
    "ChaosVehiclesEngine",
    "ChaosSolverEngine",
    "GeometryCollectionEngine",
    "FieldSystemEngine",
    "MovieScene",
    "MovieSceneTracks",
    "LevelSequence",
    "Niagara",
    "NiagaraCore",
    "NiagaraShader",
    "AudioMixer",
    "AudioExtensions",
    "MediaAssets",
    "OnlineSubsystem",
    "OnlineSubsystemUtils",
    "OnlineServicesInterface",
    "OnlineServicesCommon",
    "CommonUI",
    "CommonInput",
    "DeveloperSettings",
    "MeshDescription",
    "StaticMeshDescription",
    "Landscape",
    "Foliage",
    "AnimGraphRuntime",
    "CinematicCamera",
    "Paper2D",
    "NetCore",
    "PacketHandler",
    "ClothingSystemRuntimeInterface",
    "ClothingSystemRuntimeCommon",
    "PhysicsUtilities",
    "Renderer",
    "PropertyAccess",
    "JsonUtilities",
    "ImageWrapper",
    "AssetRegistry",
    "EngineSettings",
    "Water",
    "MetasoundEngine",
    "MetasoundFrontend",
    "MetasoundGraphCore",
    "AudioPlatformConfiguration",
    "SignificanceManager",
    "ModularGameplay",
    "GameFeatures",
    "Iris",
    "IrisCore",
    "DataRegistry",
    "StructUtils",
    "StateTreeModule",
    "MassEntity",
    "SmartObjectsModule",
    "PCG",
    "ControlRig",
    "RigVM",
    "IKRig",
    "TimeManagement",
    "Constraints",
    "ActorLayerUtilities",
    "EnhancedInputTargets",
];

#[derive(Clone, Debug, Serialize)]
pub struct ClassCandidate {
    pub name: String,
    pub cpp_name: String,
    pub package: String,
    pub kind: String,
    pub size: String,
    pub supers: Vec<String>,
    pub reasons: Vec<String>,
    pub interesting_members: Vec<String>,
    pub big_pads: Vec<String>,
    pub interesting_functions: Vec<String>,
    pub file: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SdkReport {
    pub sdk_dir: String,
    pub basic_hpp: String,
    pub files_parsed: usize,
    pub classes_parsed: usize,
    pub offsets: BTreeMap<String, String>,
    pub constants: BTreeMap<String, u64>,
    pub uobject_layout: BTreeMap<String, String>,
    pub has_chaos_vehicles: bool,
    pub pawns: Vec<ClassCandidate>,
    pub components: Vec<ClassCandidate>,
    pub game_modes: Vec<ClassCandidate>,
    pub online: Vec<ClassCandidate>,
    pub other: Vec<ClassCandidate>,
    pub changes: Vec<Change>,
    pub notes: Vec<String>,
}

fn supers_of(c: &SdkClass, by_cpp: &HashMap<String, &SdkClass>) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = c.super_cpp.clone();
    while let Some(s) = cur {
        if out.contains(&s) || out.len() > 64 {
            break;
        }
        out.push(s.clone());
        cur = by_cpp.get(&s).and_then(|p| p.super_cpp.clone());
    }
    out
}

fn has_kw(s: &str, kws: &[&str]) -> bool {
    let l = s.to_ascii_lowercase();
    kws.iter().any(|k| l.contains(k))
}

const SIM_KW: &[&str] = &[
    "vehicle", "car", "sim", "physics", "wheel", "drive", "engine", "gear", "tyre", "tire", "susp", "chassis",
    "movement", "rally",
];
const ONLINE_KW: &[&str] = &[
    "online",
    "lobby",
    "session",
    "multiplayer",
    "leaderboard",
    "timeattack",
    "time_attack",
    "challenge",
    "matchmak",
    "ranking",
    "eos",
    "server",
    "replicat",
];
const FUNC_KW: &[&str] = &[
    "reset",
    "teleport",
    "recover",
    "transform",
    "freeze",
    "pause",
    "gear",
    "respawn",
    "restart",
    "location",
    "velocity",
    "physics",
    "rewind",
    "replay",
];
const OTHER_KW: &[&str] = &[
    "vehicle", "car", "sim", "physics", "replay", "rewind", "recover", "reset", "teleport", "tyre", "tire", "wheel",
    "stage",
];

fn candidate(c: &SdkClass, supers: Vec<String>, reasons: Vec<String>) -> ClassCandidate {
    ClassCandidate {
        name: c.ue_name(),
        cpp_name: c.cpp_name.clone(),
        package: c.package().to_string(),
        kind: c.kind.clone(),
        size: format!("{:#x}", c.size),
        supers,
        reasons,
        interesting_members: c
            .members
            .iter()
            .filter(|m| !m.name.starts_with("Pad_") && (has_kw(&m.name, SIM_KW) || has_kw(&m.ty, SIM_KW)))
            .map(|m| format!("{:#06x} {} {} ({:#x})", m.offset, m.ty, m.name, m.size))
            .take(40)
            .collect(),
        big_pads: c
            .members
            .iter()
            .filter(|m| m.name.starts_with("Pad_") && m.size >= 0x40)
            .map(|m| format!("{:#06x} ({:#x} bytes)", m.offset, m.size))
            .collect(),
        interesting_functions: c.functions.iter().filter(|f| has_kw(f, FUNC_KW)).take(40).cloned().collect(),
        file: c.file.clone(),
    }
}

pub fn search_sdk(classes: &[SdkClass], report: &mut SdkReport) {
    let by_cpp: HashMap<String, &SdkClass> = classes.iter().map(|c| (c.cpp_name.clone(), c)).collect();
    report.has_chaos_vehicles = classes.iter().any(|c| c.package().starts_with("ChaosVehicles"));
    for c in classes.iter().filter(|c| !c.full_name.is_empty()) {
        let engine = ENGINE_PACKAGES.contains(&c.package());
        if engine {
            continue;
        }
        let supers = supers_of(c, &by_cpp);
        let derives = |n: &str| supers.iter().any(|s| s == n);
        let mut reasons = Vec::new();
        if derives("AWheeledVehiclePawn") {
            reasons.push("derives from AWheeledVehiclePawn (Chaos Vehicles)".to_string());
        }
        if derives("UChaosVehicleMovementComponent") || derives("UChaosWheeledVehicleMovementComponent") {
            reasons.push("derives from a Chaos vehicle movement component".to_string());
        }
        if derives("APawn") {
            reasons.push("derives from APawn".to_string());
            report.pawns.push(candidate(c, supers, reasons));
        } else if derives("UActorComponent") && has_kw(&c.cpp_name, SIM_KW) {
            reasons.push("UActorComponent with a vehicle/sim-like name".to_string());
            report.components.push(candidate(c, supers, reasons));
        } else if derives("AGameModeBase") || derives("AGameStateBase") {
            reasons.push("game mode / game state".to_string());
            report.game_modes.push(candidate(c, supers, reasons));
        } else if has_kw(&c.cpp_name, ONLINE_KW) {
            reasons.push("online/lobby/leaderboard-like name".to_string());
            report.online.push(candidate(c, supers, reasons));
        } else if has_kw(&c.cpp_name, OTHER_KW) {
            reasons.push("vehicle/sim-like name".to_string());
            report.other.push(candidate(c, supers, reasons));
        }
    }
    // Vehicle-ish pawns first, then by size (the sim car tends to be the biggest).
    let score = |c: &ClassCandidate| (!c.reasons.iter().any(|r| r.contains("Vehicle")), !has_kw(&c.cpp_name, SIM_KW));
    report
        .pawns
        .sort_by(|a, b| score(a).cmp(&score(b)).then(b.size.len().cmp(&a.size.len())).then(b.size.cmp(&a.size)));
    report.components.sort_by_key(|c| std::cmp::Reverse(c.big_pads.len()));
}

// ---- driver -----------------------------------------------------------------------------

pub fn find_basic_hpp(dir: &Path) -> Option<PathBuf> {
    fn walk(d: &Path, depth: usize) -> Option<PathBuf> {
        let p = d.join("Basic.hpp");
        if p.is_file() {
            return Some(p);
        }
        if depth == 0 {
            return None;
        }
        let mut subs: Vec<PathBuf> =
            std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        subs.iter().find_map(|s| walk(s, depth - 1))
    }
    walk(dir, 5)
}

pub struct ImportOptions {
    pub apply_classes: bool,
}

/// Parses the SDK under `dir`; returns the report and the updated signatures text.
pub fn import(dir: &Path, sigs_text: &str, o: &ImportOptions) -> Result<(SdkReport, String)> {
    let basic = find_basic_hpp(dir)
        .with_context(|| format!("no Basic.hpp under {} (expected <dir>/CppSDK/SDK/Basic.hpp)", dir.display()))?;
    let sdk_dir = basic.parent().unwrap().to_path_buf();
    let basic_text = std::fs::read_to_string(&basic)?;
    let offsets = parse_offsets(&basic_text);
    if offsets.is_empty() {
        bail!("{} has no `namespace Offsets`", basic.display());
    }
    let mut constants = BTreeMap::new();
    for n in ["FNameBlockOffsetBits", "FNameEntryStride"] {
        if let Some(v) = find_constant(&basic_text, n) {
            constants.insert(n.to_string(), v);
        }
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&sdk_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.ends_with("_classes.hpp") || f.ends_with("_structs.hpp"))
        })
        .collect();
    files.sort();
    let mut classes = parse_classes(&basic_text, "Basic.hpp");
    let parsed: Vec<Vec<SdkClass>> = files
        .par_iter()
        .filter_map(|f| {
            let t = std::fs::read_to_string(f).ok()?;
            Some(parse_classes(&t, &f.file_name()?.to_string_lossy()))
        })
        .collect();
    classes.extend(parsed.into_iter().flatten());

    let mut by_cpp: HashMap<String, &SdkClass> = HashMap::new();
    for c in &classes {
        // Prefer the definition with members (CoreUObject_classes.hpp over forward decls).
        let e = by_cpp.entry(c.cpp_name.clone()).or_insert(c);
        if e.members.is_empty() && !c.members.is_empty() {
            *e = c;
        }
    }
    let mut report = SdkReport {
        sdk_dir: sdk_dir.display().to_string(),
        basic_hpp: basic.display().to_string(),
        files_parsed: files.len() + 1,
        classes_parsed: classes.len(),
        offsets: offsets.iter().map(|(k, v)| (k.clone(), format!("{v:#x}"))).collect(),
        constants: constants.clone(),
        ..Default::default()
    };
    let planned = planned_values(&offsets, &constants, &by_cpp);
    for (path, key, v, _) in &planned {
        if path.len() == 2 && path[1] == "uobject" {
            report.uobject_layout.insert(key.clone(), format!("{v:#x}"));
        }
    }
    for (_, key) in OFFSET_KEYS {
        if !planned.iter().any(|p| &p.1 == key) {
            report
                .notes
                .push(format!("Offsets for {key} missing or 0 in Basic.hpp; the configured strategy will be used"));
        }
    }
    let configured_gnames = sigs_text
        .parse::<DocumentMut>()
        .ok()
        .and_then(|d| d.get("raw_offsets")?.get("gnames_rva")?.as_integer())
        .map(|v| v as u64);
    if let Some(n) = gnames_note(&offsets, configured_gnames) {
        eprintln!("{n}");
        report.notes.push(n);
    }
    let missing: Vec<&str> =
        UOBJECT_KEYS.iter().filter(|(_, _, k)| !report.uobject_layout.contains_key(*k)).map(|(_, _, k)| *k).collect();
    if !missing.is_empty() {
        report.notes.push(format!("uobject layout keys not found in SDK (left unchanged): {missing:?}"));
    }
    let (mut text, mut changes) = apply_values(sigs_text, &planned)?;
    search_sdk(&classes, &mut report);
    if o.apply_classes {
        let pawns: Vec<String> = report
            .pawns
            .iter()
            .filter(|c| c.reasons.len() > 1 || has_kw(&c.cpp_name, SIM_KW))
            .take(3)
            .map(|c| c.name.clone())
            .collect();
        let (t, ch) = prepend_classes(&text, &["reflection"], "vehicle_pawn_class_candidates", &pawns)?;
        text = t;
        changes.extend(ch);
        let comps: Vec<String> = report
            .components
            .iter()
            .filter(|c| c.reasons.iter().any(|r| r.contains("movement")))
            .take(3)
            .map(|c| c.name.clone())
            .collect();
        let (t, ch) = prepend_classes(&text, &["reflection"], "movement_component_class_candidates", &comps)?;
        text = t;
        changes.extend(ch);
    }
    report.changes = changes;
    Ok((report, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = r#"
#pragma once
namespace Offsets
{
	constexpr int32 GObjects          = 0x0B7C1A30;
	constexpr int32 AppendString      = 0x01234560;
	constexpr int32 GNames            = 0x0B6F0000;
	constexpr int32 GWorld            = 0x0BA00008;
	constexpr int32 ProcessEvent      = 0x01500000;
	constexpr int32 ProcessEventIdx   = 0x0000004F;
}

class FNamePool
{
public:
	static constexpr uint32 FNameEntryStride = 0x0002;
	static constexpr uint32 FNameBlockOffsetBits = 0x0010;
};

// Predefined struct FField
// 0x0038 (0x0038 - 0x0000)
class FField
{
public:
	void*                                         VTable;                                            // 0x0000(0x0008)(NOT AUTO-GENERATED PROPERTY)
	class FFieldClass*                            ClassPrivate;                                      // 0x0008(0x0008)(NOT AUTO-GENERATED PROPERTY)
	FFieldVariant                                 Owner;                                             // 0x0010(0x0010)(NOT AUTO-GENERATED PROPERTY)
	class FField*                                 Next;                                              // 0x0020(0x0008)(NOT AUTO-GENERATED PROPERTY)
	FName                                         Name;                                              // 0x0028(0x0008)(NOT AUTO-GENERATED PROPERTY)
	int32                                         ObjFlags;                                          // 0x0030(0x0004)(NOT AUTO-GENERATED PROPERTY)
};

// Predefined struct FProperty
// 0x0040 (0x0078 - 0x0038)
class FProperty : public FField
{
public:
	int32                                         ArrayDim;                                          // 0x0038(0x0004)(NOT AUTO-GENERATED PROPERTY)
	int32                                         ElementSize;                                       // 0x003C(0x0004)(NOT AUTO-GENERATED PROPERTY)
	uint64                                        PropertyFlags;                                     // 0x0040(0x0008)(NOT AUTO-GENERATED PROPERTY)
	uint8                                         Pad_48[0x4];                                       // 0x0048(0x0004)(Fixing Size After Last Property [ Dumper-7 ])
	int32                                         Offset;                                            // 0x004C(0x0004)(NOT AUTO-GENERATED PROPERTY)
};
"#;

    const CORE: &str = r#"
// Class CoreUObject.Object
// 0x0028 (0x0028 - 0x0000)
class UObject
{
public:
	static inline class TUObjectArrayWrapper      GObjects;                                          // Offset: 0x0B7C1A30, Size: 0x0028
	void*                                         VTable;                                            // 0x0000(0x0008)(NOT AUTO-GENERATED PROPERTY)
	EObjectFlags                                  Flags;                                             // 0x0008(0x0004)(NOT AUTO-GENERATED PROPERTY)
	int32                                         Index;                                             // 0x000C(0x0004)(NOT AUTO-GENERATED PROPERTY)
	class UClass*                                 Class;                                             // 0x0010(0x0008)(NOT AUTO-GENERATED PROPERTY)
	class FName                                   Name;                                              // 0x0018(0x0008)(NOT AUTO-GENERATED PROPERTY)
	class UObject*                                Outer;                                             // 0x0020(0x0008)(NOT AUTO-GENERATED PROPERTY)

public:
	static class UObject* FindObjectFastImpl(const std::string& Name, EClassCastFlags RequiredType = EClassCastFlags::None)
	{
		for (int i = 0; i < GObjects->Num(); ++i)
		{
		}
		return nullptr;
	}
	void ExecuteUbergraph(int32 EntryPoint);
};

// Class CoreUObject.Field
// 0x0008 (0x0030 - 0x0028)
class UField : public UObject
{
public:
	class UField*                                 Next;                                              // 0x0028(0x0008)(NOT AUTO-GENERATED PROPERTY)
};

// Class CoreUObject.Struct
// 0x0060 (0x0090 - 0x0030)
class UStruct : public UField
{
public:
	uint8                                         Pad_30[0x10];                                      // 0x0030(0x0010)(Fixing Size After Last Property [ Dumper-7 ])
	class UStruct*                                SuperStruct;                                       // 0x0040(0x0008)(NOT AUTO-GENERATED PROPERTY)
	class UField*                                 Children;                                          // 0x0048(0x0008)(NOT AUTO-GENERATED PROPERTY)
	class FField*                                 ChildProperties;                                   // 0x0050(0x0008)(NOT AUTO-GENERATED PROPERTY)
	int32                                         Size;                                              // 0x0058(0x0004)(NOT AUTO-GENERATED PROPERTY)
	int16                                         MinAlignment;                                      // 0x005C(0x0002)(NOT AUTO-GENERATED PROPERTY)
};

// Class CoreUObject.Function
// 0x0020 (0x00E0 - 0x00C0)
class UFunction final : public UStruct
{
public:
	uint32                                        FunctionFlags;                                     // 0x00B0(0x0004)(NOT AUTO-GENERATED PROPERTY)
	uint8                                         Pad_B4[0x24];                                      // 0x00B4(0x0024)(Fixing Size After Last Property [ Dumper-7 ])
	FNativeFuncPtr                                ExecFunction;                                      // 0x00D8(0x0008)(NOT AUTO-GENERATED PROPERTY)
};
"#;

    const GAME: &str = r#"
// Class Engine.Actor
// 0x0298 (0x02C0 - 0x0028)
class AActor : public UObject
{
};

// Class Engine.Pawn
// 0x0068 (0x0328 - 0x02C0)
class APawn : public AActor
{
};

// Class Engine.ActorComponent
// 0x00A0 (0x00C8 - 0x0028)
class UActorComponent : public UObject
{
};

// Class ChaosVehicles.WheeledVehiclePawn
// 0x0018 (0x0340 - 0x0328)
class AWheeledVehiclePawn : public APawn
{
};

// Class AcrSim.AcrCarPawn
// 0x0A00 (0x0D40 - 0x0340)
class AAcrCarPawn : public AWheeledVehiclePawn
{
public:
	class UAcrSimCarComponent*                    SimCar;                                            // 0x0340(0x0008)(Edit, ExportObject, ZeroConstructor, InstancedReference)
	uint8                                         Pad_348[0x9F8];                                    // 0x0348(0x09F8)(Fixing Struct Size After Last Property [ Dumper-7 ])

public:
	void ResetCarTransform(const struct FTransform& NewTransform, bool bTeleport);
	void SetGear(int32 Gear);
	float GetSpeedKmh() const;
};

// Class AcrSim.AcrSimCarComponent
// 0x1000 (0x10C8 - 0x00C8)
class UAcrSimCarComponent final : public UActorComponent
{
public:
	uint8                                         Pad_C8[0x1000];                                    // 0x00C8(0x1000)(Fixing Struct Size After Last Property [ Dumper-7 ])
};

// Class AcrOnline.AcrLeaderboardManager
// 0x0010 (0x0038 - 0x0028)
class UAcrLeaderboardManager : public UObject
{
public:
	void SubmitTime(float Seconds);
};
"#;

    #[test]
    fn gnames_is_reported_never_written() {
        let g = BTreeMap::from([("GNames".to_string(), 0x10u64)]);
        assert!(gnames_note(&BTreeMap::new(), Some(1)).is_none());
        assert!(gnames_note(&g, None).unwrap().contains("NOT imported"));
        assert!(gnames_note(&g, Some(0x10)).unwrap().contains("equals Dumper-7's GNames"));
        assert!(gnames_note(&g, Some(0x20)).unwrap().contains("keeping 0x20"));
        let planned =
            planned_values(&BTreeMap::from([("GNames".to_string(), 0x10u64)]), &BTreeMap::new(), &HashMap::new());
        assert!(planned.is_empty(), "{planned:?}");
    }

    #[test]
    fn parses_offsets_and_constants() {
        let o = parse_offsets(BASIC);
        assert_eq!(o["GObjects"], 0x0B7C1A30);
        assert_eq!(o["ProcessEventIdx"], 0x4F);
        assert_eq!(o.len(), 6);
        assert_eq!(find_constant(BASIC, "FNameBlockOffsetBits"), Some(16));
        assert_eq!(find_constant(BASIC, "FNameEntryStride"), Some(2));
    }

    #[test]
    fn parses_classes_members_functions() {
        let cs = parse_classes(CORE, "CoreUObject_classes.hpp");
        let names: Vec<&str> = cs.iter().map(|c| c.cpp_name.as_str()).collect();
        assert_eq!(names, ["UObject", "UField", "UStruct", "UFunction"]);
        let obj = &cs[0];
        assert_eq!(obj.full_name, "CoreUObject.Object");
        assert_eq!(obj.kind, "Class");
        assert_eq!(obj.size, 0x28);
        assert_eq!(obj.member("Name").unwrap().offset, 0x18);
        assert_eq!(obj.member("Outer").unwrap().ty, "class UObject*");
        assert!(obj.member("GObjects").is_none(), "static member comment isn't an offset");
        assert_eq!(obj.functions, ["FindObjectFastImpl", "ExecuteUbergraph"]);
        assert_eq!(cs[3].super_cpp.as_deref(), Some("UStruct"));
        assert_eq!(cs[3].member("ExecFunction").unwrap().offset, 0xD8);
        assert_eq!(cs[2].member("Pad_30").unwrap().size, 0x10);
        let b = parse_classes(BASIC, "Basic.hpp");
        let prop = b.iter().find(|c| c.cpp_name == "FProperty").unwrap();
        assert_eq!(prop.member("Offset").unwrap().offset, 0x4C);
        assert_eq!(prop.ue_name(), "Property");
    }

    fn sigs_text() -> String {
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/signatures.toml")).unwrap()
    }

    fn write_sdk(dir: &Path) {
        let sdk = dir.join("5.6.0-ACR").join("CppSDK").join("SDK");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(sdk.join("Basic.hpp"), BASIC).unwrap();
        std::fs::write(sdk.join("CoreUObject_classes.hpp"), CORE).unwrap();
        std::fs::write(sdk.join("AcrSim_classes.hpp"), GAME).unwrap();
    }

    #[test]
    fn import_updates_signatures_and_finds_candidates() {
        let dir = std::env::temp_dir().join(format!("acr_probe_d7_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_sdk(&dir);
        let original = sigs_text();
        let (rep, text) = import(&dir, &original, &ImportOptions { apply_classes: true }).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let s = acr_ue::sigs::Signatures::from_toml_str(&text).expect("updated file still parses");
        assert_eq!(s.raw_offsets.gobjects_rva, Some(0x0B7C1A30));
        assert_eq!(s.raw_offsets.gnames_rva, Some(0xb27ba80), "validated value kept, Dumper-7 GNames not imported");
        assert!(
            rep.notes.iter().any(|n| n.contains("GNames 0xb6f0000 differs") && n.contains("keeping 0xb27ba80")),
            "{:?}",
            rep.notes
        );
        assert!(!rep.changes.iter().any(|c| c.key == "raw_offsets.gnames_rva"));
        assert_eq!(s.raw_offsets.gworld_rva, Some(0x0BA00008));
        assert_eq!(s.raw_offsets.process_event_index, Some(0x4F));
        assert_eq!(s.raw_offsets.uobject.name, 0x18);
        assert_eq!(s.raw_offsets.uobject.ustruct_super, 0x40);
        assert_eq!(s.raw_offsets.uobject.ufunction_flags, 0xB0);
        assert_eq!(s.raw_offsets.uobject.ufunction_func, 0xD8);
        assert_eq!(s.raw_offsets.uobject.fproperty_offset, 0x4C);
        assert_eq!(s.fnamepool.layout.block_offset_bits, 16);
        assert!(text.contains("gobjects_rva = 0xb7c1a30"), "hex kept");
        assert!(text.contains("Dumper-7 Offsets::GObjects"), "existing comment kept");
        let first_comment_line = original.lines().find(|l| l.starts_with('#')).unwrap();
        assert!(text.contains(first_comment_line), "comments preserved");
        assert_eq!(s.reflection.vehicle_pawn_class_candidates[0], "AcrCarPawn");

        assert!(rep.has_chaos_vehicles);
        assert_eq!(rep.pawns[0].name, "AcrCarPawn");
        assert!(rep.pawns[0].interesting_functions.contains(&"ResetCarTransform".to_string()));
        assert!(rep.pawns[0].interesting_members.iter().any(|m| m.contains("SimCar")));
        assert_eq!(rep.components[0].name, "AcrSimCarComponent");
        assert_eq!(rep.components[0].big_pads.len(), 1);
        assert_eq!(rep.online[0].name, "AcrLeaderboardManager");
        assert!(rep.changes.iter().any(|c| c.key == "raw_offsets.gobjects_rva"));

        // Idempotent: a second import changes nothing.
        let dir2 = std::env::temp_dir().join(format!("acr_probe_d7b_{}", std::process::id()));
        write_sdk(&dir2);
        let (rep2, text2) = import(&dir2, &text, &ImportOptions { apply_classes: true }).unwrap();
        std::fs::remove_dir_all(&dir2).ok();
        assert!(rep2.changes.is_empty(), "{:?}", rep2.changes);
        assert_eq!(text, text2);
    }
}
