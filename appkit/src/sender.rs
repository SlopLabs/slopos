use std::marker::PhantomData;

use slopos_windowing::UiSender;

/// Sends an app its own messages from any thread, waking its loop; each
/// arrives at [`crate::App::update`] on the UI thread, in the order sent.
pub struct AppSender<M> {
    ui: UiSender,
    message: PhantomData<fn(M)>,
}

impl<M> AppSender<M> {
    pub fn new(ui: UiSender) -> Self {
        Self {
            ui,
            message: PhantomData,
        }
    }
}

impl<M: Send + 'static> AppSender<M> {
    pub fn send(&self, message: M) {
        self.ui.send(message);
    }
}

impl<M> Clone for AppSender<M> {
    fn clone(&self) -> Self {
        Self::new(self.ui.clone())
    }
}
