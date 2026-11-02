//! The pointer as the display's hardware overlay shows it. The kernel takes
//! an image or a move only from the screen's holder, and the compositor
//! starts before init hands it the screen, so a refusal for want of the
//! screen says nothing of the display: only the display's own refusal makes
//! the compositor paint the pointer itself, and until one of the two answers
//! it paints none, so an overlay and a painted pointer never show at once.

use slopos_abi::Errno;

/// What putting an image on the overlay came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Upload {
    /// The overlay shows it.
    Shown,
    /// The screen is not the compositor's now.
    NotHolder,
    /// The display has no overlay, or would not take the image.
    Refused,
}

impl Upload {
    /// What the kernel's answer `rc` to `cursor_set_image` says.
    pub fn of(rc: i64) -> Upload {
        match rc {
            0 => Upload::Shown,
            rc if rc == i64::from(Errno::EPERM.raw()) => Upload::NotHolder,
            _ => Upload::Refused,
        }
    }
}

/// What the compositor knows of the overlay.
#[derive(Default, Debug)]
pub struct Overlay {
    /// Whether the display has one, once it answered an upload.
    exists: Option<bool>,
    /// The shape it shows, as the compositor last put it there.
    shape: Option<u8>,
    /// Where the compositor last placed it.
    at: Option<(i32, i32)>,
}

impl Overlay {
    /// Whether `shape` is to be put on the overlay.
    pub fn wants_image(&self, shape: u8) -> bool {
        self.exists != Some(false) && self.shape != Some(shape)
    }

    pub fn uploaded(&mut self, shape: u8, upload: Upload) {
        match upload {
            Upload::Shown => {
                self.exists = Some(true);
                self.shape = Some(shape);
            }
            Upload::NotHolder => {}
            // An overlay that showed an image keeps it rather than a painted
            // pointer showing beside it.
            Upload::Refused => {
                self.exists.get_or_insert(false);
            }
        }
    }

    /// Whether the compositor paints the pointer into its frames.
    pub fn painted(&self) -> bool {
        self.exists == Some(false)
    }

    /// Whether the overlay is to be moved to `(x, y)`.
    pub fn wants_move(&self, x: i32, y: i32) -> bool {
        self.exists == Some(true) && self.at != Some((x, y))
    }

    pub fn moved(&mut self, x: i32, y: i32, done: bool) {
        if done {
            self.at = Some((x, y));
        }
    }

    /// Another holder of the screen may have drawn on the overlay since: the
    /// compositor's image and place are to be put there again.
    pub fn forget(&mut self) {
        self.shape = None;
        self.at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARROW: u8 = 0;
    const BEAM: u8 = 1;

    #[test]
    fn the_kernels_answers_read_as_what_they_say_of_the_display() {
        assert_eq!(Upload::of(0), Upload::Shown);
        assert_eq!(Upload::of(i64::from(Errno::EPERM.raw())), Upload::NotHolder);
        assert_eq!(Upload::of(i64::from(Errno::EINVAL.raw())), Upload::Refused);
        assert_eq!(Upload::of(i64::from(Errno::ENOMEM.raw())), Upload::Refused);
    }

    #[test]
    fn a_refusal_for_want_of_the_screen_leaves_the_overlay_to_be_tried() {
        let mut overlay = Overlay::default();
        overlay.uploaded(ARROW, Upload::NotHolder);
        assert!(
            !overlay.painted(),
            "nothing is painted before the display answers"
        );
        assert!(overlay.wants_image(ARROW));
        assert!(!overlay.wants_move(10, 10), "no overlay is known to move");

        overlay.uploaded(ARROW, Upload::Shown);
        assert!(!overlay.painted());
        assert!(!overlay.wants_image(ARROW));
        assert!(overlay.wants_move(10, 10));
    }

    #[test]
    fn a_display_that_refuses_its_first_image_has_the_pointer_painted() {
        let mut overlay = Overlay::default();
        overlay.uploaded(ARROW, Upload::Refused);
        assert!(overlay.painted());
        assert!(!overlay.wants_image(BEAM));
        assert!(!overlay.wants_move(10, 10));
    }

    #[test]
    fn an_overlay_in_use_is_never_joined_by_a_painted_pointer() {
        let mut overlay = Overlay::default();
        overlay.uploaded(ARROW, Upload::Shown);
        overlay.uploaded(BEAM, Upload::Refused);
        assert!(!overlay.painted());
        assert!(
            overlay.wants_image(BEAM),
            "the shape it lacks is tried again"
        );
        overlay.uploaded(BEAM, Upload::NotHolder);
        assert!(!overlay.painted());
        assert!(overlay.wants_image(BEAM));
    }

    #[test]
    fn a_move_is_made_again_until_one_is_taken() {
        let mut overlay = Overlay::default();
        overlay.uploaded(ARROW, Upload::Shown);
        overlay.moved(5, 6, false);
        assert!(overlay.wants_move(5, 6));
        overlay.moved(5, 6, true);
        assert!(!overlay.wants_move(5, 6));
        assert!(overlay.wants_move(5, 7));
    }

    #[test]
    fn coming_back_in_front_puts_the_image_and_place_there_again() {
        let mut overlay = Overlay::default();
        overlay.uploaded(BEAM, Upload::Shown);
        overlay.moved(40, 50, true);
        overlay.forget();
        assert!(overlay.wants_image(BEAM));
        assert!(overlay.wants_move(40, 50));
        assert!(!overlay.painted());
    }
}
