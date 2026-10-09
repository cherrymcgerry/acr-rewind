//! Lazy engine resolution after init. The first successful init can run seconds after launch,
//! before any level is loaded: GWorld is still null and most UFunctions are not linked yet.
//! A background thread keeps re-resolving what is missing (reflection reads only; no
//! ProcessEvent), and logs the current world whenever it changes.

use std::time::Duration;

/// Retry period while a function some backend needs is missing.
pub const REQUIRED_RETRY: Duration = Duration::from_secs(2);
/// Retry period when only optional functions are missing (or after a long time without progress).
pub const SLOW_RETRY: Duration = Duration::from_secs(30);
/// After this long without a newly resolved function, required ones are retried slowly and
/// optional ones not at all.
pub const PROGRESS_TIMEOUT: Duration = Duration::from_secs(600);

/// When the next UFunction retry is due. `None` = stop retrying.
pub fn retry_period(required_missing: bool, any_missing: bool, since_progress: Duration) -> Option<Duration> {
    let stalled = since_progress >= PROGRESS_TIMEOUT;
    match (required_missing, any_missing) {
        (true, _) if !stalled => Some(REQUIRED_RETRY),
        (true, _) => Some(SLOW_RETRY),
        (false, true) if !stalled => Some(SLOW_RETRY),
        _ => None,
    }
}

#[cfg(windows)]
pub mod live {
    use super::*;
    use acr_ue::Game;
    use std::sync::Arc;
    use std::time::Instant;

    /// How often the GWorld slot / current world is checked.
    const WORLD_POLL: Duration = Duration::from_secs(1);

    /// `required` = function keys a configured backend needs (retried fast).
    pub fn spawn(game: Arc<Game>, required: Vec<String>) {
        std::thread::Builder::new()
            .name("acr-rewind-engine".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&game, &required)));
            })
            .ok();
    }

    fn run(game: &Game, required: &[String]) {
        let mut last_world: Option<(usize, String)> = None;
        let mut last_progress = Instant::now();
        let mut last_try = Instant::now();
        let mut done_logged = false;
        let mut stall_logged = false;
        loop {
            std::thread::sleep(WORLD_POLL);
            if !game.gworld_located() {
                if let Some(how) = game.retry_gworld() {
                    tracing::info!("GWorld located: {how}");
                }
            }
            let world = game.world().map(|w| (w, game.ue.name(w).unwrap_or_else(|| "?".into())));
            if world != last_world {
                match &world {
                    Some((w, name)) => tracing::info!("world: {name} ({w:#x})"),
                    None => tracing::info!("world: none (GWorld null; between levels or not loaded yet)"),
                }
                last_world = world;
            }

            let missing = game.missing_functions();
            let required_missing = missing.iter().any(|k| required.contains(k));
            let Some(period) = retry_period(required_missing, !missing.is_empty(), last_progress.elapsed()) else {
                if missing.is_empty() && !done_logged {
                    tracing::info!("all UFunctions resolved");
                    done_logged = true;
                } else if !missing.is_empty() && !stall_logged {
                    tracing::info!("UFunctions still missing after {PROGRESS_TIMEOUT:?}; giving up on {missing:?}");
                    stall_logged = true;
                }
                continue;
            };
            if last_try.elapsed() < period {
                continue;
            }
            last_try = Instant::now();
            let Some(newly) = game.retry_functions(Duration::ZERO) else {
                continue;
            };
            if !newly.is_empty() {
                last_progress = Instant::now();
                let still = game.missing_functions();
                tracing::info!(
                    "UFunctions resolved: {newly:?} ({} objects indexed; still missing: {still:?})",
                    game.object_count().unwrap_or(0)
                );
            }
        }
    }

    /// Waits (bounded) until a `UWorld` exists and the GObjects count has stopped jumping, so
    /// the rest of init sees a loaded engine. Lazy resolution covers whatever is still missing.
    pub fn settle(game: &Game, max: Duration) {
        let start = Instant::now();
        let mut last = game.object_count().unwrap_or(0);
        let mut announced = false;
        loop {
            let n = game.object_count().unwrap_or(0);
            let stable = last > 0 && n.abs_diff(last) * 200 <= last;
            if game.world().is_some() && stable {
                tracing::info!(
                    "engine settled after {:.1} s: world {}, {n} objects",
                    start.elapsed().as_secs_f64(),
                    game.world_name().unwrap_or_else(|| "?".into())
                );
                break;
            }
            if start.elapsed() >= max {
                tracing::info!(
                    "engine not settled after {max:?} (world {}, {n} objects); continuing, GWorld and UFunctions \
                     are re-resolved in the background",
                    if game.world().is_some() { "present" } else { "null" }
                );
                break;
            }
            if !announced {
                tracing::info!("waiting up to {max:?} for a UWorld and a stable object count ({n} objects)");
                announced = true;
            }
            last = n;
            std::thread::sleep(Duration::from_secs(1));
        }
        if let Some(newly) = game.retry_functions(Duration::ZERO) {
            if !newly.is_empty() {
                tracing::info!("UFunctions resolved after settling: {newly:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_schedule() {
        let s = Duration::from_secs;
        assert_eq!(retry_period(true, true, s(5)), Some(REQUIRED_RETRY));
        assert_eq!(retry_period(false, true, s(5)), Some(SLOW_RETRY));
        assert_eq!(retry_period(false, false, s(5)), None, "all resolved");
        assert_eq!(retry_period(true, true, PROGRESS_TIMEOUT), Some(SLOW_RETRY), "required: keep trying slowly");
        assert_eq!(retry_period(false, true, PROGRESS_TIMEOUT), None, "optional: give up");
    }
}
