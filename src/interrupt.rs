//! Ctrl+C during a foreground recording (`devctl record`, `devctl gate`,
//! `devctl hook pre-push`). Recipes run in their own process group, so the
//! terminal's interrupt never reaches them; the handler sets a flag instead,
//! which the recipe wait loop polls (about once a second) and answers by
//! killing the recipe's process tree. Outside a recipe, Ctrl+C exits at once,
//! as it would without a handler; a second Ctrl+C always does.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The error a cancelled foreground recording ends with.
pub const INTERRUPTED: &str =
    "interrupted; the recording was cancelled. Run the command again to record";

/// Process-wide interrupt state.
#[derive(Debug, Default)]
pub struct Interrupt {
    flag: AtomicBool,
    recipes: AtomicUsize,
}

/// The state the Ctrl+C handler sets.
pub static GLOBAL: Interrupt = Interrupt::new();

/// What the handler should do with a Ctrl+C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnSignal {
    /// A recipe is running; the wait loop kills it and the command fails.
    Cancel,
    /// Nothing to clean up (or a second Ctrl+C): exit now.
    Exit,
}

impl Interrupt {
    #[must_use]
    pub const fn new() -> Interrupt {
        Interrupt {
            flag: AtomicBool::new(false),
            recipes: AtomicUsize::new(0),
        }
    }

    /// Whether Ctrl+C was pressed.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Records a Ctrl+C and says what to do about it. The flag is set before
    /// the recipe count is read, and [`RecipeGuard`] drops the count before
    /// its owner re-reads the flag, so a recipe ending at the same moment is
    /// still seen as cancelled.
    pub fn signal(&self) -> OnSignal {
        let again = self.flag.swap(true, Ordering::SeqCst);
        if again || self.recipes.load(Ordering::SeqCst) == 0 {
            OnSignal::Exit
        } else {
            OnSignal::Cancel
        }
    }

    /// Marks a recipe as running until the guard is dropped.
    #[must_use]
    pub fn recipe_running(&self) -> RecipeGuard<'_> {
        self.recipes.fetch_add(1, Ordering::SeqCst);
        RecipeGuard(self)
    }
}

/// See [`Interrupt::recipe_running`].
pub struct RecipeGuard<'a>(&'a Interrupt);

impl Drop for RecipeGuard<'_> {
    fn drop(&mut self) {
        self.0.recipes.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Installs the Ctrl+C handler for [`GLOBAL`]. A failure (e.g. a handler is
/// already installed) is logged and leaves the default behaviour.
pub fn install() {
    let result = ctrlc::set_handler(|| {
        if GLOBAL.signal() == OnSignal::Exit {
            eprintln!("error: interrupted");
            std::process::exit(130);
        }
        eprintln!("interrupted; stopping the recipe...");
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "cannot install the Ctrl+C handler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_at_once_outside_a_recipe() {
        let i = Interrupt::new();
        assert!(!i.is_set());
        assert_eq!(i.signal(), OnSignal::Exit);
        assert!(i.is_set());
    }

    #[test]
    fn cancels_a_running_recipe_then_exits_on_a_second_signal() {
        let i = Interrupt::new();
        let guard = i.recipe_running();
        assert_eq!(i.signal(), OnSignal::Cancel);
        assert!(i.is_set());
        assert_eq!(i.signal(), OnSignal::Exit);
        drop(guard);
    }

    #[test]
    fn guard_drop_restores_exit() {
        let i = Interrupt::new();
        drop(i.recipe_running());
        assert_eq!(i.signal(), OnSignal::Exit);
    }
}
