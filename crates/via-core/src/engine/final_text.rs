//! A running turn's final text (Task 4 design §2.3, §6.4): the Adapter's
//! `final_text` pieces kept inline while their escaped encoding is at most
//! 256 KiB, else written to the turn's `final_text.txt`.

use via_store::{FinalTextFile, FinalTextRef, SessionId, StoreClient, TurnNumber};

use crate::api::FINAL_TEXT_INLINE;

/// The drive's accumulation of one turn's final text.
pub(super) struct FinalText {
    /// The text while it fits inline.
    inline: String,
    /// `inline`'s encoded length, quotes included.
    encoded: usize,
    /// Set once the text passed [`FINAL_TEXT_INLINE`]: the inline text is
    /// then `null` whatever becomes of the file.
    spilled: bool,
    file: Option<FinalTextFile>,
    /// A file step failed: later pieces are dropped.
    failed: bool,
}

/// The turn's final text as its terminal carries it.
pub(super) struct Settled {
    /// The inline text; `None` once it spilled.
    pub(super) inline: Option<String>,
    /// The durable file, when the text spilled and the file is durable.
    pub(super) file: Option<FinalTextRef>,
    /// A file step failed: the turn fails `store`.
    pub(super) failed: bool,
}

impl FinalText {
    pub(super) fn new() -> Self {
        Self {
            inline: String::new(),
            encoded: 2,
            spilled: false,
            file: None,
            failed: false,
        }
    }

    /// Adds one piece. The piece that would pass [`FINAL_TEXT_INLINE`]
    /// creates `final_text.txt` and writes the held text and the piece;
    /// later pieces are appended. `Err` reports the first failed file step,
    /// once; later pieces are then dropped.
    pub(super) async fn push(
        &mut self,
        store: &StoreClient,
        (session, turn): (&SessionId, TurnNumber),
        piece: &str,
    ) -> Result<(), ()> {
        if self.failed {
            return Ok(());
        }
        if !self.spilled {
            let added = via_adapters::encoded_text_len(piece);
            if self.encoded + added <= FINAL_TEXT_INLINE {
                self.inline.push_str(piece);
                self.encoded += added;
                return Ok(());
            }
            self.spilled = true;
            let held = std::mem::take(&mut self.inline);
            let Ok(mut file) = store.final_text_file(session, turn).await else {
                self.failed = true;
                return Err(());
            };
            let written = file.append(&held).await;
            self.file = Some(file);
            if written.is_err() {
                self.failed = true;
                return Err(());
            }
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        if file.append(piece).await.is_err() {
            self.failed = true;
            return Err(());
        }
        Ok(())
    }

    /// Makes the file durable, the file then its folder, before the
    /// terminal is built (design §6.4); a failed sync names no file.
    pub(super) async fn settle(self) -> Settled {
        let file = match self.file {
            Some(file) => Some(file.finish().await),
            None => None,
        };
        Settled {
            inline: (!self.spilled).then_some(self.inline),
            failed: self.failed || file.as_ref().is_some_and(Result::is_err),
            file: file.and_then(Result::ok),
        }
    }
}
