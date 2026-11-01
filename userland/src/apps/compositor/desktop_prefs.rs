//! What the person's preferences say of their desktop: its background and
//! how its clock counts the hours. Looked at again a few times a second,
//! since nothing says when a file changed, and Settings, `prefs` and an
//! editor all change it the same way.

use std::time::{Duration, Instant};

use slopos_chrome_core::clock::Hours;
use slopos_settings_core::background::Fit;
use slopos_settings_core::prefs::Prefs;
use slopos_theme_core::Gradient;

use crate::prefs::Watch;

const LOOK_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub gradient: &'static Gradient,
    pub picture: Option<String>,
    pub fit: Fit,
}

impl Chosen {
    fn of(prefs: &Prefs) -> Chosen {
        let background = prefs.background();
        let gradient = slopos_theme_core::GRADIENTS
            .iter()
            .find(|g| g.id == background.gradient.id)
            .unwrap_or(&slopos_theme_core::GRADIENTS[0]);
        Chosen {
            gradient,
            picture: background.picture.map(str::to_string),
            fit: background.fit,
        }
    }
}

pub struct DesktopPrefs {
    watch: Option<Watch>,
    looked: Instant,
    pub background: Chosen,
    pub hours: Hours,
}

#[derive(Clone, Copy, Default)]
pub struct Changed {
    pub background: bool,
    pub hours: bool,
}

fn hours(prefs: &Prefs) -> Hours {
    match prefs.twelve_hour_clock() {
        true => Hours::Twelve,
        false => Hours::TwentyFour,
    }
}

impl DesktopPrefs {
    /// The person's, or every default on the login screen, which is nobody's.
    pub fn load(greeter: bool) -> DesktopPrefs {
        let watch = (!greeter)
            .then(crate::prefs::own_path)
            .flatten()
            .map(Watch::new);
        let prefs = watch
            .as_ref()
            .map_or_else(Prefs::default, |w| crate::prefs::read(w.path()));
        DesktopPrefs {
            watch,
            looked: Instant::now(),
            background: Chosen::of(&prefs),
            hours: hours(&prefs),
        }
    }

    /// What changed since this last looked.
    pub fn poll(&mut self) -> Changed {
        let Some(watch) = self.watch.as_mut() else {
            return Changed::default();
        };
        if self.looked.elapsed() < LOOK_EVERY {
            return Changed::default();
        }
        self.looked = Instant::now();
        if !watch.changed() {
            return Changed::default();
        }
        let prefs = crate::prefs::read(watch.path());
        let (background, hours) = (Chosen::of(&prefs), hours(&prefs));
        let changed = Changed {
            background: background != self.background,
            hours: hours != self.hours,
        };
        self.background = background;
        self.hours = hours;
        changed
    }
}
