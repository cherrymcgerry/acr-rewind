//! `config/signatures.toml` schema. See the comments in that file for the meaning of each key.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum SigsError {
    #[error("failed to parse signatures.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid signatures.toml: {0}")]
    Invalid(String),
}

pub const SUPPORTED_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Signatures {
    pub schema_version: u32,
    pub verified_build: String,
    pub target_build: String,
    pub engine_version: String,
    pub require_build_match: bool,
    pub module: ModuleSpec,
    pub gobjects: GObjectsSpec,
    pub fnamepool: FNamePoolSpec,
    pub gworld: GWorldSpec,
    pub process_event: ProcessEventSpec,
    pub world_tick: WorldTickSpec,
    pub freeze: FreezeSpec,
    pub reflection: ReflectionSpec,
    pub raw_offsets: RawOffsets,
    pub backend: BackendSpec,
    pub online_guard: OnlineGuardSpec,
}

/// Write paths, tried in `order`; the first one that can be built wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// dmphysics `CarAvatar` UFunctions (+ the vtable_scan bodies for angular velocity).
    CarAvatar,
    /// Raw sim memory (`[raw_offsets.sim_car]`).
    SimCar,
    /// Generic AActor teleport through ProcessEvent.
    UeActor,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct BackendSpec {
    pub order: Vec<BackendKind>,
    pub car_avatar: CarAvatarSpec,
}

impl Default for BackendSpec {
    fn default() -> Self {
        Self {
            order: vec![BackendKind::CarAvatar, BackendKind::SimCar, BackendKind::UeActor],
            car_avatar: CarAvatarSpec::default(),
        }
    }
}

/// Which sim-body fields `car_avatar` writes after `SetPhysicsTransform` / `SetVelocityCMS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyWrites {
    None,
    /// Recorded linear + angular velocity of every body (zero while scrubbing).
    #[default]
    Velocities,
    /// The full recorded state (pose + velocities) of every body.
    Full,
}

/// `"auto"` or three signed UE axes, e.g. `["+y", "+z", "+x"]`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum AxesSpec {
    Named(String),
    Map(Vec<String>),
}

impl Default for AxesSpec {
    fn default() -> Self {
        Self::Named("auto".into())
    }
}

impl AxesSpec {
    /// `Ok(None)` = auto-detect.
    pub fn parse(&self) -> Result<Option<crate::math::AxisMap>, String> {
        match self {
            Self::Named(s) if s.eq_ignore_ascii_case("auto") => Ok(None),
            Self::Named(s) => Err(format!("ue_to_sim_axes: expected \"auto\" or [\"+x\", ...], got \"{s}\"")),
            Self::Map(v) => crate::math::AxisMap::parse(v).map(Some),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CarAvatarSpec {
    /// The pawn must be an instance of (a subclass of) this class.
    pub pawn_class: String,
    /// `[reflection.functions]` keys that must resolve for this backend, on top of the ones it
    /// can't work without (`car_avatar::CORE_FUNCTIONS`).
    pub required_functions: Vec<String>,
    /// `bResetCar` passed to SetPhysicsTransform.
    pub reset_car: bool,
    pub body_writes: BodyWrites,
    pub ue_to_sim_axes: AxesSpec,
    /// Auto-detection tolerance between the actor location and the main sim body (m).
    pub axes_fit_tolerance_m: f64,
    /// GetGear returns an engine-allocated FString; poll it at most this often.
    pub gear_poll_ms: u64,
    /// Interval of the `car_avatar validate:` diagnostic log line (0 = off).
    pub diag_interval_ms: u64,
}

impl Default for CarAvatarSpec {
    fn default() -> Self {
        Self {
            pawn_class: "CarAvatar".into(),
            required_functions: Vec::new(),
            reset_car: false,
            body_writes: BodyWrites::Velocities,
            ue_to_sim_axes: AxesSpec::default(),
            axes_fit_tolerance_m: 1.0,
            gear_poll_ms: 1000,
            diag_interval_ms: 2000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ModuleSpec {
    pub name: String,
    pub original_filename: String,
    pub scan_sections: Vec<String>,
}

impl Default for ModuleSpec {
    fn default() -> Self {
        Self { name: "acr.exe".into(), original_filename: String::new(), scan_sections: vec![".text".into()] }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    #[default]
    RipRelative,
    CallRel32,
    Direct,
}

/// One pattern candidate (shared by every section that scans code).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Candidate {
    pub pattern: String,
    pub kind: CandidateKind,
    pub disp_offset: usize,
    pub instr_len: usize,
    pub post_add: i64,
    pub deref: bool,
    pub occurrence: usize,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct GObjectsSpec {
    /// "pattern" | "heuristic" | "offset"
    pub strategy: String,
    pub fallback_heuristic: bool,
    pub layout: GObjectsLayout,
    pub candidates: Vec<Candidate>,
}

impl Default for GObjectsSpec {
    fn default() -> Self {
        Self {
            strategy: "pattern".into(),
            fallback_heuristic: true,
            layout: GObjectsLayout::default(),
            candidates: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct GObjectsLayout {
    pub objects_offset: usize,
    pub max_elements_offset: usize,
    pub num_elements_offset: usize,
    pub max_chunks_offset: usize,
    pub num_chunks_offset: usize,
    pub elements_per_chunk: usize,
    pub fuobjectitem_size: usize,
    pub fuobjectitem_object_offset: usize,
}

impl Default for GObjectsLayout {
    fn default() -> Self {
        Self {
            objects_offset: 0x00,
            max_elements_offset: 0x10,
            num_elements_offset: 0x14,
            max_chunks_offset: 0x18,
            num_chunks_offset: 0x1C,
            elements_per_chunk: 65536,
            fuobjectitem_size: 0x18,
            fuobjectitem_object_offset: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FNamePoolSpec {
    /// "pattern" | "dumper7" | "offset" | "data_scan"
    pub strategy: String,
    pub fallback_pattern: bool,
    /// Last resort after the RVA / strategy: scan `.data` for the `Blocks[0]` pointer.
    pub fallback_data_scan: bool,
    pub layout: FNamePoolLayout,
    pub candidates: Vec<Candidate>,
}

impl Default for FNamePoolSpec {
    fn default() -> Self {
        Self {
            strategy: "dumper7".into(),
            fallback_pattern: true,
            fallback_data_scan: true,
            layout: FNamePoolLayout::default(),
            candidates: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct FNamePoolLayout {
    pub blocks_offset: usize,
    pub current_block_offset: usize,
    pub current_byte_cursor_offset: usize,
    pub block_offset_bits: u32,
    pub entry_stride: usize,
    pub header_size: usize,
    pub header_len_shift: u32,
    pub header_wide_mask: u16,
}

impl Default for FNamePoolLayout {
    fn default() -> Self {
        Self {
            blocks_offset: 0x10,
            current_block_offset: 0x08,
            current_byte_cursor_offset: 0x0C,
            block_offset_bits: 16,
            entry_stride: 2,
            header_size: 2,
            header_len_shift: 6,
            header_wide_mask: 0x1,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct GWorldSpec {
    /// "pattern" | "reflection" | "offset"
    pub strategy: String,
    pub fallback_pattern: bool,
    pub candidates: Vec<Candidate>,
}

impl Default for GWorldSpec {
    fn default() -> Self {
        Self { strategy: "reflection".into(), fallback_pattern: true, candidates: Vec::new() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ProcessEventSpec {
    /// "vtable_index" | "flags_scan" | "pattern" | "offset"
    pub strategy: String,
    pub vtable_index: Option<usize>,
    pub max_vtable_scan: usize,
    pub candidates: Vec<Candidate>,
}

impl Default for ProcessEventSpec {
    fn default() -> Self {
        Self { strategy: "flags_scan".into(), vtable_index: None, max_vtable_scan: 0x150, candidates: Vec::new() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct WorldTickSpec {
    /// "process_event" | "vtable" | "pattern" | "sim_step"
    pub strategy: String,
    /// "present_counter" | "qpc_16ms"
    pub frame_source: String,
    pub vtable_class: String,
    pub vtable_index: Option<usize>,
    pub candidates: Vec<Candidate>,
}

impl Default for WorldTickSpec {
    fn default() -> Self {
        Self {
            strategy: "process_event".into(),
            frame_source: "present_counter".into(),
            vtable_class: "GameEngine".into(),
            vtable_index: None,
            candidates: Vec::new(),
        }
    }
}

/// How the simulation is frozen while scrubbing. Tried in order; the first lever that can be
/// resolved is used.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct FreezeSpec {
    /// Any of "sim_step", "custom_time_dilation", "global_time_dilation", "none".
    pub levers: Vec<String>,
    /// Value written for "global_time_dilation" (the engine clamps to MinGlobalTimeDilation).
    pub global_time_dilation_value: f32,
}

impl Default for FreezeSpec {
    fn default() -> Self {
        Self {
            levers: vec!["sim_step".into(), "custom_time_dilation".into(), "global_time_dilation".into()],
            global_time_dilation_value: 0.0001,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FunctionRef {
    pub class: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ReflectionSpec {
    /// "player_pawn" | "class_scan"
    pub pawn_lookup: String,
    pub player_index: i32,
    pub vehicle_pawn_class_candidates: Vec<String>,
    pub root_component_property: String,
    pub physics_component_property_candidates: Vec<String>,
    pub movement_component_class_candidates: Vec<String>,
    pub time_dilation_property: String,
    pub functions: BTreeMap<String, FunctionRef>,
    pub required_functions: Vec<String>,
}

impl Default for ReflectionSpec {
    fn default() -> Self {
        Self {
            pawn_lookup: "player_pawn".into(),
            player_index: 0,
            vehicle_pawn_class_candidates: Vec::new(),
            root_component_property: "RootComponent".into(),
            physics_component_property_candidates: Vec::new(),
            movement_component_class_candidates: Vec::new(),
            time_dilation_property: "CustomTimeDilation".into(),
            functions: BTreeMap::new(),
            required_functions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawOffsets {
    pub gobjects_rva: Option<u64>,
    pub gnames_rva: Option<u64>,
    pub gworld_rva: Option<u64>,
    pub process_event_rva: Option<u64>,
    pub process_event_index: Option<usize>,
    pub append_string_rva: Option<u64>,
    pub sim_step_rva: Option<u64>,
    pub uobject: UObjectLayout,
    pub sim_car: SimCarSpec,
}

/// Member offsets of the core reflection types.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub struct UObjectLayout {
    pub vtable: usize,
    pub object_flags: usize,
    pub internal_index: usize,
    pub class: usize,
    pub name: usize,
    pub outer: usize,
    pub ufield_next: usize,
    pub ustruct_super: usize,
    pub ustruct_children: usize,
    pub ustruct_child_properties: usize,
    pub ustruct_properties_size: usize,
    pub ufunction_flags: usize,
    pub ufunction_func: usize,
    pub ffield_class: usize,
    pub ffield_next: usize,
    pub ffield_name: usize,
    pub fproperty_element_size: usize,
    pub fproperty_flags: usize,
    pub fproperty_offset: usize,
}

impl Default for UObjectLayout {
    fn default() -> Self {
        Self {
            vtable: 0x00,
            object_flags: 0x08,
            internal_index: 0x0C,
            class: 0x10,
            name: 0x18,
            outer: 0x20,
            ufield_next: 0x28,
            ustruct_super: 0x40,
            ustruct_children: 0x48,
            ustruct_child_properties: 0x50,
            ustruct_properties_size: 0x58,
            ufunction_flags: 0xB0,
            ufunction_func: 0xD8,
            ffield_class: 0x08,
            ffield_next: 0x20,
            ffield_name: 0x28,
            fproperty_element_size: 0x3C,
            fproperty_flags: 0x40,
            fproperty_offset: 0x4C,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainRoot {
    Module,
    #[default]
    Pawn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    F32,
    #[default]
    F64,
}

impl ValueType {
    pub fn size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixLayout {
    /// Consecutive values are the rows (basis vectors laid out row by row).
    #[default]
    Rows,
    Columns,
}

/// How the sim car is found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimCarLocator {
    /// Scan private RW memory for rigid-body objects (vtable `vtable_rva`), group them by
    /// their parent pointer and pick the group at the shm car position.
    #[default]
    VtableScan,
    /// Follow `chain` from `chain_root`.
    Chain,
}

/// Custom-solver car state (see re-notes §6.B). With `locator = "vtable_scan"` every field
/// offset is relative to the start of a rigid-body object (the vtable pointer).
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SimCarSpec {
    pub locator: SimCarLocator,
    /// RVA of the rigid-body vtable (`vtable_scan`).
    pub vtable_rva: Option<u64>,
    /// Size of one rigid-body object; every field must lie inside it.
    pub object_size: usize,
    /// Offset of the parent (world / car) pointer shared by all bodies of one car.
    pub parent_offset: usize,
    /// Max distance (m) between a body and the shm car position for the car to match, plus
    /// `|v| * shm_lag_s`.
    pub match_tolerance_m: f64,
    /// How far (s) shm graphics carCoordinates trails the physics. Every body-vs-shm position
    /// check allows `|v| * shm_lag_s` on top of its base tolerance.
    pub shm_lag_s: f64,
    /// Only bodies of the parent group within this distance (m) of the main body at lock
    /// time belong to the car; the parent pointer is shared with unrelated world objects.
    pub car_radius_m: f64,
    /// While the main body moves faster than `car_moving_mps`, a body also needs a linear
    /// velocity within this (m/s, plus |w| x distance) of the main body's.
    pub car_velocity_tolerance_mps: f64,
    pub car_moving_mps: f64,
    /// Minimum time between two background scans.
    pub rescan_interval_ms: u64,
    /// Max movement (m) of the main body between two ticks before the cache is distrusted.
    pub max_jump_m: f64,
    /// Rescan if the main body stays further than this from the shm car position...
    pub hint_mismatch_m: f64,
    /// ...for this long while we are not writing.
    pub hint_mismatch_s: f64,
    pub chain_root: ChainRoot,
    pub chain: Vec<u64>,
    pub value_type: ValueType,
    /// Simulation length units per metre (1.0 = metres, 100.0 = centimetres).
    pub length_scale: f64,
    pub position: Option<usize>,
    pub rotation_matrix: Option<usize>,
    pub rotation_matrix_layout: MatrixLayout,
    /// Elements per matrix row in memory (3 for a 3x3, 4 for a 4x4 / 3x4 with padding).
    pub rotation_matrix_row_stride: usize,
    pub orientation_quat: Option<usize>,
    pub linear_velocity: Option<usize>,
    pub angular_velocity: Option<usize>,
    pub wheel_angular_speed: Vec<usize>,
    pub engine_rpm: Option<usize>,
    /// True if `engine_rpm` actually stores engine angular speed in rad/s.
    pub engine_rpm_is_rad_s: bool,
    pub gear: Option<usize>,
    /// Value stored in `gear` for neutral (AC shm uses 1 = N, 0 = R).
    pub gear_neutral_value: i32,
    pub blob_regions: Vec<[usize; 2]>,
}

impl Default for SimCarSpec {
    fn default() -> Self {
        Self {
            locator: SimCarLocator::VtableScan,
            vtable_rva: None,
            object_size: 0x300,
            parent_offset: 0x8,
            match_tolerance_m: 2.0,
            shm_lag_s: 0.06,
            car_radius_m: 4.0,
            car_velocity_tolerance_mps: 3.0,
            car_moving_mps: 1.0,
            rescan_interval_ms: 1000,
            max_jump_m: 20.0,
            hint_mismatch_m: 5.0,
            hint_mismatch_s: 0.5,
            chain_root: ChainRoot::Pawn,
            chain: Vec::new(),
            value_type: ValueType::F64,
            length_scale: 1.0,
            position: None,
            rotation_matrix: None,
            rotation_matrix_layout: MatrixLayout::Rows,
            rotation_matrix_row_stride: 3,
            orientation_quat: None,
            linear_velocity: None,
            angular_velocity: None,
            wheel_angular_speed: Vec::new(),
            engine_rpm: None,
            engine_rpm_is_rad_s: false,
            gear: None,
            gear_neutral_value: 0,
            blob_regions: Vec::new(),
        }
    }
}

impl SimCarSpec {
    /// True when enough is known to read and restore a pose: a way to find the car (vtable
    /// or chain), a position and an orientation (`vtable_scan` needs the matrix).
    pub fn is_configured(&self) -> bool {
        let located = match self.locator {
            SimCarLocator::VtableScan => self.vtable_rva.is_some() && self.rotation_matrix.is_some(),
            SimCarLocator::Chain => self.chain_root == ChainRoot::Module || !self.chain.is_empty(),
        };
        located && self.position.is_some() && (self.rotation_matrix.is_some() || self.orientation_quat.is_some())
    }

    /// Bytes from the object start that cover every typed field (one read per body).
    pub fn body_span(&self) -> usize {
        let s = self.value_type.size();
        let rot = self.rotation_matrix.map(|o| o + (2 * self.rotation_matrix_row_stride + 3) * s);
        [
            Some(self.parent_offset + 8),
            self.position.map(|o| o + 3 * s),
            rot,
            self.linear_velocity.map(|o| o + 3 * s),
            self.angular_velocity.map(|o| o + 3 * s),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(8)
    }

    /// Fields that are configured but would make a resume incomplete.
    pub fn missing_for_full_restore(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.linear_velocity.is_none() {
            m.push("linear_velocity");
        }
        if self.angular_velocity.is_none() {
            m.push("angular_velocity");
        }
        if self.wheel_angular_speed.len() != 4 {
            m.push("wheel_angular_speed[4]");
        }
        if self.engine_rpm.is_none() {
            m.push("engine_rpm");
        }
        if self.gear.is_none() {
            m.push("gear");
        }
        m
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OnlineGuardSpec {
    pub enabled: bool,
    pub fail_closed: bool,
    pub recheck_interval_ms: u64,
    pub require_standalone_net_mode: bool,
    pub shm_require_status_live: bool,
    pub shm_blocked_session_types: Vec<i32>,
    pub shm_block_if_static_is_online: bool,
    pub shm_moza_block_if_server_name_nonempty: bool,
    /// Block if `numVehicles` exceeds this. Absent = signal not used.
    pub shm_moza_block_if_num_vehicles_gt: Option<i32>,
    pub blocked_live_classes: Vec<String>,
    pub blocked_live_class_name_substrings: Vec<String>,
    pub block_leaderboard_modes: bool,
    pub leaderboard_mode_class_name_substrings: Vec<String>,
}

impl Default for OnlineGuardSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            fail_closed: true,
            recheck_interval_ms: 1000,
            require_standalone_net_mode: true,
            shm_require_status_live: false,
            shm_blocked_session_types: Vec::new(),
            shm_block_if_static_is_online: true,
            shm_moza_block_if_server_name_nonempty: true,
            shm_moza_block_if_num_vehicles_gt: Some(1),
            blocked_live_classes: Vec::new(),
            blocked_live_class_name_substrings: Vec::new(),
            block_leaderboard_modes: true,
            leaderboard_mode_class_name_substrings: Vec::new(),
        }
    }
}

impl Signatures {
    pub fn from_toml_str(s: &str) -> Result<Self, SigsError> {
        let sigs: Self = toml::from_str(s)?;
        sigs.validate()?;
        Ok(sigs)
    }

    pub fn validate(&self) -> Result<(), SigsError> {
        let invalid = |m: String| Err(SigsError::Invalid(m));
        if self.schema_version != SUPPORTED_SCHEMA {
            return invalid(format!(
                "schema_version {} unsupported (expected {SUPPORTED_SCHEMA})",
                self.schema_version
            ));
        }
        if self.module.name.trim().is_empty() {
            return invalid("module.name is empty".into());
        }
        let all_candidates = self
            .gobjects
            .candidates
            .iter()
            .map(|c| ("gobjects", c))
            .chain(self.fnamepool.candidates.iter().map(|c| ("fnamepool", c)))
            .chain(self.gworld.candidates.iter().map(|c| ("gworld", c)))
            .chain(self.process_event.candidates.iter().map(|c| ("process_event", c)))
            .chain(self.world_tick.candidates.iter().map(|c| ("world_tick", c)));
        for (section, c) in all_candidates {
            let pat = crate::pattern::Pattern::parse(&c.pattern)
                .map_err(|e| SigsError::Invalid(format!("[{section}] pattern: {e}")))?;
            if c.kind != CandidateKind::Direct && (c.instr_len == 0 || c.disp_offset + 4 > c.instr_len.max(pat.len())) {
                return invalid(format!("[{section}] candidate '{}': disp_offset/instr_len inconsistent", c.pattern));
            }
        }
        const REFLECTION_KEYS: &[&str] = &[
            "pawn_lookup",
            "player_index",
            "vehicle_pawn_class_candidates",
            "root_component_property",
            "physics_component_property_candidates",
            "movement_component_class_candidates",
            "time_dilation_property",
            "required_functions",
        ];
        if let Some(k) = self.reflection.functions.keys().find(|k| REFLECTION_KEYS.contains(&k.as_str())) {
            return invalid(format!(
                "[reflection.functions] contains '{k}', which belongs in [reflection] (move it above the \
                 [reflection.functions] header)"
            ));
        }
        for name in &self.reflection.required_functions {
            if !self.reflection.functions.contains_key(name) {
                return invalid(format!("reflection.required_functions references unknown function '{name}'"));
            }
        }
        let sc = &self.raw_offsets.sim_car;
        if !(sc.length_scale.is_finite() && sc.length_scale > 0.0) {
            return invalid("raw_offsets.sim_car.length_scale must be > 0".into());
        }
        if sc.rotation_matrix.is_some() && !(3..=4).contains(&sc.rotation_matrix_row_stride) {
            return invalid("raw_offsets.sim_car.rotation_matrix_row_stride must be 3 or 4".into());
        }
        if !sc.wheel_angular_speed.is_empty() && sc.wheel_angular_speed.len() != 4 {
            return invalid("raw_offsets.sim_car.wheel_angular_speed needs exactly 4 offsets".into());
        }
        for [off, len] in &sc.blob_regions {
            if *len == 0 || *len > 0x10000 || off.checked_add(*len).is_none() {
                return invalid(format!(
                    "raw_offsets.sim_car.blob_regions entry [{off:#x}, {len:#x}] invalid (len 1..=0x10000)"
                ));
            }
        }
        if sc.locator == SimCarLocator::VtableScan {
            let positive = |v: f64| v.is_finite() && v > 0.0;
            if !(positive(sc.match_tolerance_m)
                && positive(sc.max_jump_m)
                && positive(sc.hint_mismatch_m)
                && positive(sc.car_radius_m)
                && positive(sc.car_velocity_tolerance_mps))
                || !(sc.hint_mismatch_s.is_finite() && sc.hint_mismatch_s >= 0.0)
                || !(sc.car_moving_mps.is_finite() && sc.car_moving_mps >= 0.0)
                || !(sc.shm_lag_s.is_finite() && (0.0..=1.0).contains(&sc.shm_lag_s))
            {
                return invalid(
                    "raw_offsets.sim_car: match_tolerance_m, max_jump_m, hint_mismatch_m, car_radius_m, \
                     car_velocity_tolerance_mps must be > 0 (shm_lag_s 0..=1 s)"
                        .into(),
                );
            }
            if sc.rescan_interval_ms == 0 {
                return invalid("raw_offsets.sim_car.rescan_interval_ms must be > 0".into());
            }
            if sc.object_size < 0x10 || sc.body_span() > sc.object_size {
                return invalid(format!(
                    "raw_offsets.sim_car: fields span {:#x} bytes but object_size is {:#x}",
                    sc.body_span(),
                    sc.object_size
                ));
            }
            if sc.orientation_quat.is_some() {
                return invalid("raw_offsets.sim_car: vtable_scan supports rotation_matrix only".into());
            }
        }
        let ca = &self.backend.car_avatar;
        if self.backend.order.is_empty() {
            return invalid("backend.order is empty".into());
        }
        ca.ue_to_sim_axes.parse().map_err(|e| SigsError::Invalid(format!("backend.car_avatar.{e}")))?;
        if !(ca.axes_fit_tolerance_m.is_finite() && ca.axes_fit_tolerance_m > 0.0) {
            return invalid("backend.car_avatar.axes_fit_tolerance_m must be > 0".into());
        }
        for name in &ca.required_functions {
            if !self.reflection.functions.contains_key(name) {
                return invalid(format!("backend.car_avatar.required_functions references unknown function '{name}'"));
            }
        }
        if self.online_guard.recheck_interval_ms == 0 {
            return invalid("online_guard.recheck_interval_ms must be > 0".into());
        }
        Ok(())
    }

    /// Whether the running build matches `verified_build`. `None` if either is unknown.
    pub fn build_matches(&self, running_build: Option<&str>) -> Option<bool> {
        let running = running_build?.trim();
        let verified = self.verified_build.trim();
        if running.is_empty() || verified.is_empty() {
            return None;
        }
        Some(running == verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../../config/signatures.toml");

    #[test]
    fn shipped_signatures_parse() {
        let s = Signatures::from_toml_str(SHIPPED).unwrap();
        assert_eq!(s.schema_version, 1);
        assert_eq!(s.module.name, "acr.exe");
        assert_eq!(s.target_build, "25170642");
        assert_eq!(s.verified_build, "25170642");
        assert_eq!(s.gobjects.candidates.len(), 1);
        assert_eq!(s.gobjects.candidates[0].kind, CandidateKind::RipRelative);
        assert_eq!(s.gobjects.layout.elements_per_chunk, 65536);
        assert_eq!(s.fnamepool.strategy, "dumper7");
        assert_eq!(s.process_event.strategy, "flags_scan");
        assert_eq!(s.process_event.vtable_index, Some(0x4C));
        assert_eq!(s.world_tick.strategy, "process_event");
        assert_eq!(s.freeze.levers, ["sim_step", "custom_time_dilation", "global_time_dilation"]);
        assert!(!s.online_guard.blocked_live_classes.iter().any(|c| c == "OnlineSession"));
        assert!(s.reflection.functions.contains_key("get_player_pawn"));
        assert_eq!(s.reflection.functions["k2_set_actor_location_and_rotation"].name, "K2_SetActorLocationAndRotation");
        assert_eq!(s.raw_offsets.gobjects_rva, Some(0xb35f5b0));
        assert_eq!(s.raw_offsets.gnames_rva, Some(0xb27ba80), "live FNamePool, not Dumper-7 GNames");
        assert_eq!(
            s.reflection.required_functions,
            ["k2_get_actor_location", "k2_get_actor_rotation", "k2_set_actor_location_and_rotation", "get_player_pawn"]
        );
        assert!(!s.reflection.functions.contains_key("required_functions"));
        assert_eq!(s.raw_offsets.sim_car.car_radius_m, 4.0);
        assert_eq!(s.raw_offsets.gworld_rva, Some(0xb074730));
        assert_eq!(s.raw_offsets.process_event_rva, Some(0x16971b0));
        assert_eq!(s.raw_offsets.process_event_index, Some(0x4c));
        assert_eq!(s.raw_offsets.append_string_rva, Some(0x14769e0));
        let u = &s.raw_offsets.uobject;
        assert_eq!(
            (u.ffield_next, u.ffield_name, u.fproperty_element_size, u.fproperty_flags, u.fproperty_offset),
            (0x18, 0x20, 0x34, 0x38, 0x44)
        );
        assert_eq!((u.ufield_next, u.ustruct_children, u.ufunction_func), (0x28, 0x48, 0xD8), "untouched by the merge");
        assert_eq!(s.reflection.pawn_lookup, "local_player");
        assert_eq!(&s.reflection.vehicle_pawn_class_candidates[..2], ["AcrCarAvatar", "CarAvatar"]);
        assert_eq!(s.backend.order, [BackendKind::CarAvatar, BackendKind::SimCar, BackendKind::UeActor]);
        let ca = &s.backend.car_avatar;
        assert_eq!(ca.pawn_class, "CarAvatar");
        assert!(!ca.reset_car);
        assert_eq!(ca.body_writes, BodyWrites::Velocities);
        let pinned = ca.ue_to_sim_axes.parse().unwrap().expect("pinned in-game, not auto");
        assert_eq!((pinned.axis, pinned.sign), ([0, 2, 1], [1, 1, 1]), "+x +z +y");
        for k in
            crate::car_avatar::CORE_FUNCTIONS.iter().copied().chain(ca.required_functions.iter().map(String::as_str))
        {
            assert!(s.reflection.functions.contains_key(k), "{k}");
        }
        assert_eq!(s.reflection.functions["car_set_physics_transform"].class, "CarAvatar");
        let sc = &s.raw_offsets.sim_car;
        assert!(sc.is_configured());
        assert_eq!(sc.locator, SimCarLocator::VtableScan);
        assert_eq!(sc.vtable_rva, Some(0xa8d9830));
        assert_eq!((sc.object_size, sc.parent_offset), (0x300, 0x8));
        assert_eq!(sc.position, Some(0x138));
        assert_eq!(sc.rotation_matrix, Some(0x158));
        assert_eq!(sc.rotation_matrix_row_stride, 4);
        assert_eq!(sc.linear_velocity, Some(0x1d8));
        assert_eq!(sc.angular_velocity, Some(0x1f8));
        assert_eq!(sc.value_type, ValueType::F64);
        assert_eq!(sc.body_span(), 0x210);
        assert!(!s.online_guard.shm_require_status_live);
        assert!(s.online_guard.enabled && s.online_guard.fail_closed);
        assert_eq!(s.online_guard.shm_moza_block_if_num_vehicles_gt, Some(1));
        assert_eq!(s.build_matches(Some("25170642")), Some(true));
    }

    #[test]
    fn rejects_bad_schema_and_patterns() {
        assert!(Signatures::from_toml_str("schema_version = 2").is_err());
        let bad = "schema_version = 1\n[[gobjects.candidates]]\npattern = \"48 ZZ\"\n";
        assert!(matches!(Signatures::from_toml_str(bad), Err(SigsError::Invalid(_))));
        let bad_disp = "schema_version = 1\n[[gobjects.candidates]]\npattern = \"48 8B 05 ?? ?? ?? ??\"\nkind = \"rip_relative\"\ndisp_offset = 5\ninstr_len = 7\n";
        assert!(Signatures::from_toml_str(bad_disp).is_err());
        let unknown_req = "schema_version = 1\n[reflection]\nrequired_functions = [\"nope\"]\n";
        assert!(Signatures::from_toml_str(unknown_req).is_err());
    }

    #[test]
    fn required_functions_under_the_functions_header_is_rejected() {
        // The shipped file had exactly this: the list parsed as a function named
        // "required_functions" (class/name = its first two elements) and nothing was required.
        let misplaced = "schema_version = 1\n[reflection.functions]\n\
            a = { class = \"Actor\", name = \"A\" }\nb = { class = \"Actor\", name = \"B\" }\n\
            required_functions = [\"a\", \"b\"]\n";
        match Signatures::from_toml_str(misplaced) {
            Err(SigsError::Invalid(e)) => assert!(e.contains("belongs in [reflection]"), "{e}"),
            Err(e) => assert!(e.to_string().contains("required_functions"), "{e}"),
            Ok(_) => panic!("accepted a misplaced required_functions"),
        }
    }

    #[test]
    fn backend_section() {
        let s = Signatures::from_toml_str(
            "schema_version = 1\n[backend]\norder = [\"sim_car\"]\n[backend.car_avatar]\nue_to_sim_axes = [\"+y\", \"-z\", \"+x\"]\nbody_writes = \"full\"\nreset_car = true\n",
        )
        .unwrap();
        assert_eq!(s.backend.order, [BackendKind::SimCar]);
        let ca = &s.backend.car_avatar;
        let m = ca.ue_to_sim_axes.parse().unwrap().unwrap();
        assert_eq!((m.axis, m.sign), ([1, 2, 0], [1, -1, 1]));
        assert_eq!(ca.body_writes, BodyWrites::Full);
        assert!(ca.reset_car);
        let defaults = Signatures::from_toml_str("schema_version = 1").unwrap();
        assert_eq!(defaults.backend.order.len(), 3);
        assert_eq!(defaults.backend.car_avatar.body_writes, BodyWrites::Velocities);
        for bad in [
            "[backend]\norder = []",
            "[backend.car_avatar]\nue_to_sim_axes = \"sideways\"",
            "[backend.car_avatar]\nue_to_sim_axes = [\"+x\", \"+x\", \"+z\"]",
            "[backend.car_avatar]\naxes_fit_tolerance_m = 0.0",
            "[backend.car_avatar]\nrequired_functions = [\"nope\"]",
            "[backend]\norder = [\"teleport\"]",
        ] {
            assert!(Signatures::from_toml_str(&format!("schema_version = 1\n{bad}\n")).is_err(), "{bad}");
        }
    }

    #[test]
    fn sim_car_overrides() {
        let s = Signatures::from_toml_str(
            r#"
schema_version = 1
verified_build = "25170642"
[raw_offsets]
gobjects_rva = 0x1234
[raw_offsets.sim_car]
locator = "chain"
chain_root = "module"
chain = [0x10, 0x20]
value_type = "f32"
position = 0x40
rotation_matrix = 0x50
rotation_matrix_row_stride = 4
wheel_angular_speed = [0x100, 0x104, 0x108, 0x10C]
blob_regions = [[0x200, 0x80]]
"#,
        )
        .unwrap();
        let sc = &s.raw_offsets.sim_car;
        assert_eq!(s.raw_offsets.gobjects_rva, Some(0x1234));
        assert!(sc.is_configured());
        assert_eq!(sc.chain_root, ChainRoot::Module);
        assert_eq!(sc.value_type, ValueType::F32);
        assert_eq!(sc.blob_regions, vec![[0x200, 0x80]]);
        assert!(sc.missing_for_full_restore().contains(&"linear_velocity"));
        assert_eq!(s.build_matches(Some("25170642")), Some(true));
        assert_eq!(s.build_matches(Some("1")), Some(false));
    }

    #[test]
    fn rejects_bad_sim_car() {
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nwheel_angular_speed = [1, 2]\n";
        assert!(Signatures::from_toml_str(s).is_err());
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nblob_regions = [[0, 0]]\n";
        assert!(Signatures::from_toml_str(s).is_err());
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nposition = 0x2F8\n";
        assert!(Signatures::from_toml_str(s).is_err(), "field outside object_size");
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nmatch_tolerance_m = 0.0\n";
        assert!(Signatures::from_toml_str(s).is_err());
        let s = "schema_version = 1\n[raw_offsets.sim_car]\norientation_quat = 0x10\n";
        assert!(Signatures::from_toml_str(s).is_err());
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nlocator = \"chain\"\nposition = 0x2F8\n";
        assert!(Signatures::from_toml_str(s).is_ok(), "chain mode has no object bound");
        let s = "schema_version = 1\n[raw_offsets.sim_car]\nposition = 0x138\nrotation_matrix = 0x158\n";
        let sc = Signatures::from_toml_str(s).unwrap().raw_offsets.sim_car;
        assert!(!sc.is_configured(), "vtable_scan needs vtable_rva");
    }
}
