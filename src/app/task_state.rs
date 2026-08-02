use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayPhase {
    Idle,
    Running,
}

/// Owns the lifecycle of the single display-formatting task.
pub(super) struct DisplayTaskState {
    revision: u64,
    phase: DisplayPhase,
    full_rebuild: bool,
    last_started: Instant,
}

impl DisplayTaskState {
    pub(super) fn new(debounce: Duration) -> Self {
        Self {
            revision: 0,
            phase: DisplayPhase::Idle,
            full_rebuild: true,
            last_started: Instant::now() - debounce,
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.phase == DisplayPhase::Running
    }

    pub(super) fn needs_full_rebuild(&self) -> bool {
        self.full_rebuild
    }

    pub(super) fn debounce_remaining(&self, debounce: Duration) -> Option<Duration> {
        (!self.full_rebuild)
            .then(|| debounce.saturating_sub(self.last_started.elapsed()))
            .filter(|remaining| !remaining.is_zero())
    }

    pub(super) fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.full_rebuild = true;
    }

    pub(super) fn require_full_rebuild(&mut self) {
        self.full_rebuild = true;
    }

    pub(super) fn start(&mut self) -> u64 {
        debug_assert_eq!(self.phase, DisplayPhase::Idle);
        self.phase = DisplayPhase::Running;
        self.full_rebuild = false;
        self.last_started = Instant::now();
        self.revision
    }

    /// Finishes the active task and reports whether its result still belongs to the UI state.
    pub(super) fn finish(&mut self, revision: u64, may_apply: bool) -> bool {
        debug_assert_eq!(self.phase, DisplayPhase::Running);
        self.phase = DisplayPhase::Idle;
        let current = revision == self.revision && may_apply;
        if !current {
            self.full_rebuild = true;
        }
        current
    }

    pub(super) fn mark_applied(&mut self) {
        self.full_rebuild = false;
        self.last_started = Instant::now();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchRun {
    revision: u64,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchPhase {
    Idle,
    Debouncing {
        not_before: Instant,
    },
    Ready,
    Running {
        run: SearchRun,
        rerun_not_before: Option<Instant>,
    },
}

/// Serializes search tasks while retaining the latest request made during an active run.
pub(super) struct SearchTaskState {
    revision: u64,
    phase: SearchPhase,
    reset_selection: bool,
}

impl SearchTaskState {
    pub(super) fn new() -> Self {
        Self {
            revision: 0,
            phase: SearchPhase::Idle,
            reset_selection: false,
        }
    }

    pub(super) fn is_running(&self) -> bool {
        matches!(self.phase, SearchPhase::Running { .. })
    }

    pub(super) fn is_pending(&self) -> bool {
        matches!(
            self.phase,
            SearchPhase::Debouncing { .. }
                | SearchPhase::Ready
                | SearchPhase::Running {
                    rerun_not_before: Some(_),
                    ..
                }
        )
    }

    pub(super) fn is_busy(&self) -> bool {
        self.is_running() || self.is_pending()
    }

    pub(super) fn request_debounced(&mut self, debounce: Duration, reset_selection: bool) {
        self.revision = self.revision.wrapping_add(1);
        self.reset_selection |= reset_selection;
        self.queue(Instant::now() + debounce);
    }

    pub(super) fn request_now(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.queue(Instant::now());
    }

    fn queue(&mut self, not_before: Instant) {
        self.phase = match self.phase {
            SearchPhase::Running { run, .. } => SearchPhase::Running {
                run,
                rerun_not_before: Some(not_before),
            },
            _ if not_before > Instant::now() => SearchPhase::Debouncing { not_before },
            _ => SearchPhase::Ready,
        };
    }

    /// Invalidates an active result and removes any queued rerun.
    pub(super) fn cancel(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.reset_selection = false;
        self.phase = match self.phase {
            SearchPhase::Running { run, .. } => SearchPhase::Running {
                run,
                rerun_not_before: None,
            },
            _ => SearchPhase::Idle,
        };
    }

    pub(super) fn debounce_remaining(&self) -> Option<Duration> {
        match self.phase {
            SearchPhase::Debouncing { not_before } => {
                Some(not_before.saturating_duration_since(Instant::now()))
                    .filter(|remaining| !remaining.is_zero())
            }
            _ => None,
        }
    }

    pub(super) fn start(&mut self, generation: u64) -> Option<(u64, u64)> {
        let ready = match self.phase {
            SearchPhase::Ready => true,
            SearchPhase::Debouncing { not_before } => Instant::now() >= not_before,
            _ => false,
        };
        if !ready {
            return None;
        }

        let run = SearchRun {
            revision: self.revision,
            generation,
        };
        self.phase = SearchPhase::Running {
            run,
            rerun_not_before: None,
        };
        Some((run.revision, run.generation))
    }

    /// Completes the active run. A stale run either yields to the queued request or schedules an
    /// immediate rerun when only the display generation changed.
    pub(super) fn finish(
        &mut self,
        revision: u64,
        generation: u64,
        current_generation: u64,
        matcher_available: bool,
    ) -> bool {
        let SearchPhase::Running {
            run,
            rerun_not_before,
        } = self.phase
        else {
            debug_assert!(false, "search result arrived without an active task");
            return false;
        };
        debug_assert_eq!((run.revision, run.generation), (revision, generation));

        let current = revision == self.revision && generation == current_generation;
        self.phase = if current {
            SearchPhase::Idle
        } else if let Some(not_before) = rerun_not_before {
            if not_before > Instant::now() {
                SearchPhase::Debouncing { not_before }
            } else {
                SearchPhase::Ready
            }
        } else if revision == self.revision && matcher_available {
            SearchPhase::Ready
        } else {
            SearchPhase::Idle
        };
        current
    }

    pub(super) fn abort(&mut self, revision: u64, generation: u64) {
        let SearchPhase::Running { run, .. } = self.phase else {
            debug_assert!(false, "cannot abort a search that is not running");
            return;
        };
        debug_assert_eq!((run.revision, run.generation), (revision, generation));
        self.phase = SearchPhase::Idle;
        self.reset_selection = false;
    }

    pub(super) fn take_reset_selection(&mut self) -> bool {
        std::mem::take(&mut self.reset_selection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalidation_during_run_rejects_the_old_result() {
        let mut state = DisplayTaskState::new(Duration::ZERO);
        let revision = state.start();
        state.invalidate();

        assert!(!state.finish(revision, true));
        assert!(state.needs_full_rebuild());
        assert!(!state.is_running());
    }

    #[test]
    fn search_request_during_run_is_debounced_then_rerun() {
        let mut state = SearchTaskState::new();
        state.request_now();
        let (revision, generation) = state.start(4).unwrap();
        state.request_debounced(Duration::from_secs(1), true);

        assert!(state.is_running());
        assert!(state.is_pending());
        assert!(!state.finish(revision, generation, generation, true));
        assert!(!state.is_running());
        assert!(state.is_pending());
        assert!(state.debounce_remaining().is_some());
    }

    #[test]
    fn cancelling_while_running_discards_the_result_without_a_rerun() {
        let mut state = SearchTaskState::new();
        state.request_now();
        let (revision, generation) = state.start(8).unwrap();
        state.cancel();

        assert!(!state.finish(revision, generation, generation, false));
        assert!(!state.is_busy());
        assert!(!state.take_reset_selection());
    }

    #[test]
    fn generation_change_requeues_the_current_search_immediately() {
        let mut state = SearchTaskState::new();
        state.request_now();
        let (revision, generation) = state.start(3).unwrap();

        assert!(!state.finish(revision, generation, 4, true));
        assert!(state.is_pending());
        assert!(state.debounce_remaining().is_none());
        assert_eq!(state.start(4), Some((revision, 4)));
    }

    #[test]
    fn abort_returns_the_search_state_to_idle() {
        let mut state = SearchTaskState::new();
        state.request_now();
        let (revision, generation) = state.start(5).unwrap();

        state.abort(revision, generation);

        assert!(!state.is_busy());
    }
}
