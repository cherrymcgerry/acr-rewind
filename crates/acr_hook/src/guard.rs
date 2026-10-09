//! Online / leaderboard guard. Multi-signal and fail-closed (`[online_guard]`).

use acr_shm::{MozaInfo, Status};
use acr_ue::sigs::OnlineGuardSpec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Rewind may run.
    Allowed,
    /// Not driving right now (paused, replay, menu): keep history, skip updates.
    Inactive(String),
    /// Online / leaderboard / unknown: the mod must be off and history discarded.
    Blocked(String),
}

/// Raw signal values; `None` = could not be evaluated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Signals {
    pub net_standalone: Option<bool>,
    pub shm_status: Option<Status>,
    pub shm_session: Option<i32>,
    pub static_is_online: Option<bool>,
    pub moza: Option<MozaInfo>,
    /// `Some(Some(class))` = a blocked class is live.
    pub blocked_live_class: Option<Option<String>>,
    /// `Some(None)` = no game mode object (e.g. front-end).
    pub game_mode_class: Option<Option<String>>,
    pub game_state_class: Option<Option<String>>,
    /// Name of the current UWorld (log context only; not evaluated).
    pub world: Option<String>,
}

pub fn evaluate(spec: &OnlineGuardSpec, s: &Signals) -> Verdict {
    if !spec.enabled {
        return Verdict::Allowed;
    }
    macro_rules! unknown {
        ($what:expr) => {
            if spec.fail_closed {
                return Verdict::Blocked(format!("{} unavailable (fail closed)", $what));
            }
        };
    }

    if spec.require_standalone_net_mode {
        match s.net_standalone {
            Some(true) => {}
            Some(false) => return Verdict::Blocked("UE net mode is not standalone".into()),
            None => unknown!("UE net mode"),
        }
    }
    if spec.shm_block_if_static_is_online {
        match s.static_is_online {
            Some(false) => {}
            Some(true) => return Verdict::Blocked("shared memory reports an online session".into()),
            None => unknown!("shared memory (static page)"),
        }
    }
    if !spec.shm_blocked_session_types.is_empty() {
        match s.shm_session {
            Some(t) if spec.shm_blocked_session_types.contains(&t) => {
                return Verdict::Blocked(format!("blocked session type {t}"))
            }
            Some(_) => {}
            None => unknown!("shared memory (session type)"),
        }
    }
    let moza_needed = spec.shm_moza_block_if_server_name_nonempty || spec.shm_moza_block_if_num_vehicles_gt.is_some();
    // The acpmf_Moza page only exists while Moza software runs: absent = no signal (not
    // "unknown"); present = checked.
    if moza_needed {
        if let Some(m) = &s.moza {
            if spec.shm_moza_block_if_server_name_nonempty && !m.server_name.is_empty() {
                return Verdict::Blocked(format!("online server '{}'", m.server_name));
            }
            if let Some(max) = spec.shm_moza_block_if_num_vehicles_gt {
                if m.num_vehicles > max {
                    return Verdict::Blocked(format!("{} vehicles in session", m.num_vehicles));
                }
            }
        }
    }
    if !spec.blocked_live_classes.is_empty() {
        match &s.blocked_live_class {
            Some(None) => {}
            Some(Some(c)) => return Verdict::Blocked(format!("online class {c} is live")),
            None => unknown!("object scan"),
        }
    }
    let check_modes = !spec.blocked_live_class_name_substrings.is_empty()
        || (spec.block_leaderboard_modes && !spec.leaderboard_mode_class_name_substrings.is_empty());
    if check_modes {
        for (what, v) in [("game mode", &s.game_mode_class), ("game state", &s.game_state_class)] {
            match v {
                None => unknown!(what),
                Some(None) => {}
                Some(Some(name)) => {
                    if let Some(sub) =
                        spec.blocked_live_class_name_substrings.iter().find(|sub| name.contains(sub.as_str()))
                    {
                        return Verdict::Blocked(format!("online {what} {name} (matches '{sub}')"));
                    }
                    if spec.block_leaderboard_modes {
                        if let Some(sub) =
                            spec.leaderboard_mode_class_name_substrings.iter().find(|sub| name.contains(sub.as_str()))
                        {
                            return Verdict::Blocked(format!("leaderboard {what} {name} (matches '{sub}')"));
                        }
                    }
                }
            }
        }
    }
    if spec.shm_require_status_live {
        match s.shm_status {
            Some(Status::Live) => {}
            Some(other) => return Verdict::Inactive(format!("game status {other}")),
            None => unknown!("shared memory (status)"),
        }
    } else if s.shm_status == Some(Status::Replay) {
        // ACR reports Off while driving, so only an explicit Replay means "not driving".
        return Verdict::Inactive(format!("game status {}", Status::Replay));
    }
    Verdict::Allowed
}

/// One line per configured check with what it saw, for the guard log.
pub fn reasons(spec: &OnlineGuardSpec, s: &Signals) -> String {
    if !spec.enabled {
        return "guard disabled".into();
    }
    let opt = |v: Option<String>| v.unwrap_or_else(|| "UNKNOWN".into());
    let mut out = Vec::new();
    if spec.require_standalone_net_mode {
        out.push(format!(
            "net mode {}",
            opt(s.net_standalone.map(|b| if b { "standalone".into() } else { "NOT standalone".into() }))
        ));
    }
    if spec.shm_block_if_static_is_online {
        out.push(format!("static isOnline {}", opt(s.static_is_online.map(|b| b.to_string()))));
    }
    if !spec.shm_blocked_session_types.is_empty() {
        out.push(format!("session {}", opt(s.shm_session.map(|t| t.to_string()))));
    }
    if spec.shm_moza_block_if_server_name_nonempty || spec.shm_moza_block_if_num_vehicles_gt.is_some() {
        out.push(match &s.moza {
            None => "Moza page absent (ignored)".into(),
            Some(m) => format!("Moza server '{}' vehicles {}", m.server_name, m.num_vehicles),
        });
    }
    if !spec.blocked_live_classes.is_empty() {
        out.push(format!(
            "blocked live class {}",
            match &s.blocked_live_class {
                None => "UNKNOWN".into(),
                Some(None) => "none".into(),
                Some(Some(c)) => c.clone(),
            }
        ));
    }
    for (what, v) in [("game mode", &s.game_mode_class), ("game state", &s.game_state_class)] {
        out.push(format!(
            "{what} {}",
            match v {
                None => "UNKNOWN".into(),
                Some(None) => "none".into(),
                Some(Some(c)) => c.clone(),
            }
        ));
    }
    out.push(format!("status {}", opt(s.shm_status.map(|st| st.to_string()))));
    out.push(format!("world {}", opt(s.world.clone())));
    out.join(", ")
}

#[cfg(windows)]
pub mod monitor {
    //! Background thread that gathers [`Signals`] and publishes a [`Verdict`].

    use super::*;
    use acr_shm::{MozaPage, SharedMemory};
    use acr_ue::game::{find_live_blocked_class, world_is_standalone, world_member_class};
    use acr_ue::Game;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    pub struct Published {
        pub verdict: Verdict,
        pub at: Instant,
    }

    /// An unchanged verdict is re-logged (with its signals) this often.
    const STATUS_PERIOD: Duration = Duration::from_secs(30);

    static LATEST: Mutex<Option<Published>> = Mutex::new(None);
    static GAME: OnceLock<Arc<Game>> = OnceLock::new();

    pub fn set_game(game: Arc<Game>) {
        let _ = GAME.set(game);
    }

    /// Latest verdict; stale or missing data blocks.
    pub fn current(interval: Duration) -> Verdict {
        let Ok(g) = LATEST.lock() else {
            return Verdict::Blocked("online guard state poisoned".into());
        };
        match g.as_ref() {
            Some(p) if p.at.elapsed() <= interval * 3 + Duration::from_secs(1) => p.verdict.clone(),
            Some(_) => Verdict::Blocked("online guard stale".into()),
            None => Verdict::Blocked("online guard not evaluated yet".into()),
        }
    }

    pub fn spawn(spec: OnlineGuardSpec) {
        std::thread::Builder::new().name("acr-rewind-guard".into()).spawn(move || run(spec)).ok();
    }

    fn run(spec: OnlineGuardSpec) {
        let interval = Duration::from_millis(spec.recheck_interval_ms);
        let mut shm: Option<SharedMemory> = None;
        let mut moza: Option<MozaPage> = None;
        let mut class_cache: HashMap<usize, bool> = HashMap::new();
        let mut last: Option<Verdict> = None;
        let mut last_status = Instant::now();
        loop {
            let started = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if shm.is_none() {
                    shm = SharedMemory::open().ok();
                }
                if moza.is_none() {
                    moza = MozaPage::open().ok();
                }
                let mut s = Signals::default();
                if let Some(m) = &shm {
                    let g = m.graphics();
                    s.shm_status = Some(g.status);
                    s.shm_session = Some(g.session);
                    let st = m.static_info();
                    s.static_is_online = Some(st.is_online);
                }
                s.moza = moza.as_ref().map(MozaPage::read);
                if let Some(game) = GAME.get() {
                    // Read fresh every recheck: GWorld is null early and changes per level.
                    if let Some(world) = game.world() {
                        s.world = game.ue.name(world);
                        s.net_standalone = world_is_standalone(&game.ue, world);
                        s.game_mode_class = world_member_class(&game.ue, world, "AuthorityGameMode");
                        s.game_state_class = world_member_class(&game.ue, world, "GameState");
                    }
                    if !spec.blocked_live_classes.is_empty() {
                        s.blocked_live_class =
                            find_live_blocked_class(&game.ue, &spec.blocked_live_classes, &mut class_cache);
                    }
                }
                (evaluate(&spec, &s), s)
            }));
            let verdict = match result {
                Ok((v, s)) => {
                    let why = reasons(&spec, &s);
                    if last.as_ref() != Some(&v) {
                        match &last {
                            Some(prev) => tracing::info!("online guard: {prev:?} -> {v:?} | {why}"),
                            None => tracing::info!("online guard: {v:?} | {why}"),
                        }
                        last_status = Instant::now();
                    } else if last_status.elapsed() >= STATUS_PERIOD {
                        tracing::info!("online guard status: {v:?} | {why}");
                        last_status = Instant::now();
                    }
                    v
                }
                Err(_) => Verdict::Blocked("online guard panicked".into()),
            };
            last = Some(verdict.clone());
            if let Ok(mut g) = LATEST.lock() {
                *g = Some(Published { verdict, at: Instant::now() });
            }
            std::thread::sleep(interval.saturating_sub(started.elapsed()).max(Duration::from_millis(50)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Mutator = Box<dyn Fn(&mut Signals)>;

    fn spec() -> OnlineGuardSpec {
        acr_ue::Signatures::from_toml_str(include_str!("../../../config/signatures.toml")).unwrap().online_guard
    }

    fn offline() -> Signals {
        Signals {
            net_standalone: Some(true),
            shm_status: Some(Status::Live),
            shm_session: Some(0),
            static_is_online: Some(false),
            moza: Some(MozaInfo { num_vehicles: 1, focus_vehicle: 0, server_name: String::new() }),
            blocked_live_class: Some(None),
            game_mode_class: Some(Some("BP_FreeRoamGameMode_C".into())),
            game_state_class: Some(None),
            world: Some("GreeceS3Elatia".into()),
        }
    }

    #[test]
    fn offline_free_roam_is_allowed() {
        assert_eq!(evaluate(&spec(), &offline()), Verdict::Allowed);
    }

    #[test]
    fn each_online_signal_blocks() {
        let sp = spec();
        let cases: Vec<Mutator> = vec![
            Box::new(|s| s.net_standalone = Some(false)),
            Box::new(|s| s.static_is_online = Some(true)),
            Box::new(|s| s.moza.as_mut().unwrap().server_name = "EU #1".into()),
            Box::new(|s| s.moza.as_mut().unwrap().num_vehicles = 8),
            Box::new(|s| s.blocked_live_class = Some(Some("ACRLobbySubsystem".into()))),
            Box::new(|s| s.game_mode_class = Some(Some("ACRMultiplayerGameMode".into()))),
            Box::new(|s| s.game_mode_class = Some(Some("BP_TimeAttackMode_C".into()))),
            Box::new(|s| s.game_state_class = Some(Some("ChallengeGameState".into()))),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut s = offline();
            f(&mut s);
            assert!(matches!(evaluate(&sp, &s), Verdict::Blocked(_)), "case {i}: {s:?}");
        }
    }

    #[test]
    fn missing_signals_fail_closed() {
        let sp = spec();
        let cases: Vec<Mutator> = vec![
            Box::new(|s| s.net_standalone = None),
            Box::new(|s| s.static_is_online = None),
            Box::new(|s| s.blocked_live_class = None),
            Box::new(|s| s.game_mode_class = None),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut s = offline();
            f(&mut s);
            match evaluate(&sp, &s) {
                Verdict::Blocked(r) => assert!(r.contains("fail closed"), "case {i}: {r}"),
                v => panic!("case {i}: {v:?}"),
            }
        }
        assert!(matches!(evaluate(&sp, &Signals::default()), Verdict::Blocked(_)));
    }

    /// Signals observed live on an offline stage (no Moza software running).
    fn offline_stage() -> Signals {
        Signals {
            net_standalone: Some(true),
            shm_status: Some(Status::Off),
            shm_session: Some(0),
            static_is_online: Some(false),
            moza: None,
            blocked_live_class: Some(None),
            game_mode_class: Some(Some("BC_RaceGameMode_C".into())),
            game_state_class: Some(Some("BC_RaceGameState_C".into())),
            world: Some("GreeceS3Elatia".into()),
        }
    }

    #[test]
    fn missing_moza_page_is_not_a_signal() {
        let sp = spec();
        assert_eq!(evaluate(&sp, &offline_stage()), Verdict::Allowed);
        let menu = Signals {
            game_mode_class: Some(Some("BC_MainGameMode_C".into())),
            game_state_class: Some(Some("BC_MainGameState_C".into())),
            world: Some("MainMenuWales".into()),
            ..offline_stage()
        };
        assert_eq!(evaluate(&sp, &menu), Verdict::Allowed);
        // A present page is still checked.
        let mut s = offline_stage();
        s.moza = Some(MozaInfo { num_vehicles: 1, focus_vehicle: 0, server_name: "EU #1".into() });
        assert!(matches!(evaluate(&sp, &s), Verdict::Blocked(_)));
        // Unknown net mode still fails closed.
        let mut s = offline_stage();
        s.net_standalone = None;
        match evaluate(&sp, &s) {
            Verdict::Blocked(r) => assert!(r.contains("UE net mode unavailable (fail closed)"), "{r}"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn reasons_list_every_check() {
        let r = reasons(&spec(), &offline_stage());
        assert_eq!(
            r,
            "net mode standalone, static isOnline false, Moza page absent (ignored), blocked live class none, \
             game mode BC_RaceGameMode_C, game state BC_RaceGameState_C, status Off, world GreeceS3Elatia"
        );
        let r = reasons(&spec(), &Signals::default());
        assert!(r.starts_with("net mode UNKNOWN, static isOnline UNKNOWN"), "{r}");
    }

    #[test]
    fn fail_open_and_disabled() {
        let mut sp = spec();
        sp.fail_closed = false;
        let mut s = offline();
        s.moza = None;
        s.net_standalone = None;
        assert_eq!(evaluate(&sp, &s), Verdict::Allowed);
        // A definite online signal still blocks when failing open.
        s.static_is_online = Some(true);
        assert!(matches!(evaluate(&sp, &s), Verdict::Blocked(_)));
        sp.enabled = false;
        assert_eq!(evaluate(&sp, &s), Verdict::Allowed);
    }

    #[test]
    fn status_is_non_blocking_except_replay_by_default() {
        let sp = spec();
        assert!(!sp.shm_require_status_live);
        // ACR reports Off while driving live; Off / Pause / missing status must not stop rewind.
        for st in [Some(Status::Off), Some(Status::Live), Some(Status::Pause), None] {
            let mut s = offline();
            s.shm_status = st;
            assert_eq!(evaluate(&sp, &s), Verdict::Allowed, "{st:?}");
        }
        let mut s = offline();
        s.shm_status = Some(Status::Replay);
        assert!(matches!(evaluate(&sp, &s), Verdict::Inactive(_)));
        // Blocked wins over inactive.
        s.net_standalone = Some(false);
        assert!(matches!(evaluate(&sp, &s), Verdict::Blocked(_)));
    }

    #[test]
    fn require_status_live_opt_in() {
        let mut sp = spec();
        sp.shm_require_status_live = true;
        for st in [Status::Pause, Status::Replay, Status::Off] {
            let mut s = offline();
            s.shm_status = Some(st);
            assert!(matches!(evaluate(&sp, &s), Verdict::Inactive(_)), "{st:?}");
        }
        let mut s = offline();
        s.shm_status = None;
        match evaluate(&sp, &s) {
            Verdict::Blocked(r) => assert!(r.contains("fail closed"), "{r}"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn session_type_blocklist() {
        let mut sp = spec();
        sp.shm_blocked_session_types = vec![7];
        let mut s = offline();
        s.shm_session = Some(7);
        assert!(matches!(evaluate(&sp, &s), Verdict::Blocked(_)));
        s.shm_session = Some(1);
        assert_eq!(evaluate(&sp, &s), Verdict::Allowed);
    }
}
