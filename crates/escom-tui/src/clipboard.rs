use crate::i18n::Message;
use crate::msg;

/// Initialize on first use and retain ownership for Linux clipboard backends.
#[derive(Default)]
pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
}

impl Clipboard {
    pub fn set_text(&mut self, text: String) -> Result<(), Message> {
        if self.inner.is_none() {
            self.inner = Some(arboard::Clipboard::new().map_err(|e| msg!(ClipboardError, e))?);
        }
        self.inner
            .as_mut()
            .expect("clipboard initialized")
            .set_text(text)
            .map_err(|e| msg!(ClipboardError, e))
    }
}
