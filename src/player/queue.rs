//! Play queue state machine. Pure data structure - no I/O - so the
//! repeat/radio/reorder rules are unit-testable.
//!
//! Model: one `Vec<Track>` plus a `current` cursor. Items before `current`
//! are history (kept visible), items after are upcoming.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::api::models::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "关",
            RepeatMode::All => "全部循环",
            RepeatMode::One => "单曲循环",
        }
    }
}

/// What the app should do after a track ends or a manual skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    Play(usize),
    Stop,
}

#[derive(Default)]
pub struct Queue {
    items: Vec<Track>,
    current: Option<usize>,
    pub repeat: RepeatMode,
}

impl Queue {
    pub fn items(&self) -> &[Track] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    #[cfg(test)]
    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.items.get(i))
    }

    pub fn track_at(&self, i: usize) -> Option<&Track> {
        self.items.get(i)
    }

    /// Number of tracks after the current one (radio refill trigger input).
    pub fn upcoming_count(&self) -> usize {
        match self.current {
            Some(c) => self.items.len().saturating_sub(c + 1),
            None => self.items.len(),
        }
    }

    /// Every videoId in the queue - used to dedupe radio continuations.
    pub fn known_ids(&self) -> HashSet<String> {
        self.items.iter().map(|t| t.video_id.clone()).collect()
    }

    /// Make `tracks` the playback context and start at `start`.
    ///
    /// This is what picking a song inside an album/playlist does: the rest
    /// of that list becomes the queue, so playback continues in order
    /// instead of running dry and being filled with radio suggestions.
    /// Returns the index to load, or None if the input is unusable.
    pub fn set_context(&mut self, tracks: Vec<Track>, start: usize) -> Option<usize> {
        if tracks.is_empty() || start >= tracks.len() {
            return None;
        }
        self.items = tracks;
        self.current = Some(start);
        Some(start)
    }

    pub fn restore(
        &mut self,
        tracks: Vec<Track>,
        current: Option<usize>,
        repeat: RepeatMode,
    ) -> bool {
        if tracks.is_empty() || current.is_some_and(|i| i >= tracks.len()) {
            return false;
        }
        self.items = tracks;
        self.current = current;
        self.repeat = repeat;
        true
    }
    /// Insert right after current without moving the cursor.
    pub fn play_next(&mut self, track: Track) {
        self.insert_after_current(track);
    }

    pub fn append(&mut self, track: Track) {
        self.items.push(track);
    }

    /// Append with dedupe against everything already queued.
    /// Returns how many were actually added.
    pub fn append_unique(&mut self, tracks: Vec<Track>) -> usize {
        let known = self.known_ids();
        let mut added = 0;
        for t in tracks {
            if !known.contains(&t.video_id)
                && !self.items[self.items.len() - added..]
                    .iter()
                    .any(|x| x.video_id == t.video_id)
            {
                self.items.push(t);
                added += 1;
            }
        }
        added
    }

    fn insert_after_current(&mut self, track: Track) -> usize {
        let at = match self.current {
            Some(c) => c + 1,
            None => self.items.len(),
        };
        self.items.insert(at, track);
        at
    }

    /// Jump to an arbitrary queue position (Enter on the queue panel).
    pub fn jump_to(&mut self, i: usize) -> Option<usize> {
        if i < self.items.len() {
            self.current = Some(i);
            Some(i)
        } else {
            None
        }
    }

    /// Remove a non-current item. Removing the playing track is refused
    /// (returns false) to keep cursor/playback consistent.
    pub fn remove(&mut self, i: usize) -> bool {
        if i >= self.items.len() || Some(i) == self.current {
            return false;
        }
        self.items.remove(i);
        if let Some(c) = self.current {
            if i < c {
                self.current = Some(c - 1);
            }
        }
        true
    }

    /// Swap item `i` with `i-1` (up=true) or `i+1`, fixing the cursor.
    pub fn swap(&mut self, i: usize, up: bool) -> Option<usize> {
        let j = if up {
            i.checked_sub(1)?
        } else {
            if i + 1 >= self.items.len() {
                return None;
            }
            i + 1
        };
        self.items.swap(i, j);
        if let Some(c) = self.current {
            if c == i {
                self.current = Some(j);
            } else if c == j {
                self.current = Some(i);
            }
        }
        Some(j)
    }

    /// Shuffle only the upcoming part; history and current stay in place.
    pub fn shuffle_upcoming(&mut self) {
        use rand::seq::SliceRandom;
        let start = match self.current {
            Some(c) => c + 1,
            None => 0,
        };
        if start < self.items.len() {
            self.items[start..].shuffle(&mut rand::rng());
        }
    }

    /// Track finished naturally (end-file: eof).
    pub fn advance_on_end(&mut self) -> Advance {
        let Some(c) = self.current else {
            return Advance::Stop;
        };
        match self.repeat {
            RepeatMode::One => Advance::Play(c),
            _ => {
                if c + 1 < self.items.len() {
                    self.current = Some(c + 1);
                    Advance::Play(c + 1)
                } else if self.repeat == RepeatMode::All && !self.items.is_empty() {
                    self.current = Some(0);
                    Advance::Play(0)
                } else {
                    Advance::Stop
                }
            }
        }
    }

    /// Manual next ("n") - RepeatMode::One is intentionally ignored here.
    pub fn next_manual(&mut self) -> Advance {
        let Some(c) = self.current else {
            return if self.items.is_empty() {
                Advance::Stop
            } else {
                self.current = Some(0);
                Advance::Play(0)
            };
        };
        if c + 1 < self.items.len() {
            self.current = Some(c + 1);
            Advance::Play(c + 1)
        } else if self.repeat == RepeatMode::All && !self.items.is_empty() {
            self.current = Some(0);
            Advance::Play(0)
        } else {
            Advance::Stop
        }
    }

    /// Manual previous ("p"). The caller handles the restart-if-mid-track UX.
    pub fn prev_manual(&mut self) -> Advance {
        let Some(c) = self.current else {
            return Advance::Stop;
        };
        if c > 0 {
            self.current = Some(c - 1);
            Advance::Play(c - 1)
        } else if self.repeat == RepeatMode::All && !self.items.is_empty() {
            let last = self.items.len() - 1;
            self.current = Some(last);
            Advance::Play(last)
        } else {
            Advance::Play(c) // restart first track
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str) -> Track {
        Track {
            video_id: id.into(),
            title: format!("song {id}"),
            artists: "artist".into(),
            album: None,
            duration_secs: Some(200),
            cover_url: None,
        }
    }

    #[test]
    fn play_next_keeps_cursor() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.jump_to(0);
        q.play_next(t("b"));
        q.play_next(t("c"));
        let order: Vec<_> = q.items().iter().map(|x| x.video_id.as_str()).collect();
        assert_eq!(order, ["a", "c", "b"]);
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn advance_repeat_modes() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        q.jump_to(1);

        q.repeat = RepeatMode::Off;
        assert_eq!(q.advance_on_end(), Advance::Stop);

        q.repeat = RepeatMode::All;
        assert_eq!(q.advance_on_end(), Advance::Play(0));

        q.repeat = RepeatMode::One;
        assert_eq!(q.advance_on_end(), Advance::Play(0));
    }

    #[test]
    fn manual_next_skips_repeat_one() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        q.jump_to(0);
        q.repeat = RepeatMode::One;
        assert_eq!(q.next_manual(), Advance::Play(1));
    }

    #[test]
    fn remove_adjusts_cursor_and_protects_current() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        q.append(t("c"));
        q.jump_to(1);
        assert!(!q.remove(1), "removing current must be refused");
        assert!(q.remove(0));
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.current_track().unwrap().video_id, "b");
    }

    #[test]
    fn swap_moves_cursor_with_items() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        q.jump_to(0);
        q.swap(0, false);
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current_track().unwrap().video_id, "a");
    }

    #[test]
    fn append_unique_dedupes() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        let added = q.append_unique(vec![t("b"), t("c"), t("c"), t("d")]);
        assert_eq!(added, 2);
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn set_context_queues_whole_list_and_plays_in_order() {
        let mut q = Queue::default();
        q.append(t("old"));
        q.jump_to(0);

        let list = vec![t("a"), t("b"), t("c"), t("d")];
        assert_eq!(q.set_context(list, 1), Some(1));
        assert_eq!(q.current_track().unwrap().video_id, "b");
        // The previous one-off queue is replaced by the new context.
        assert_eq!(q.len(), 4);
        // Playing on continues down the list rather than running dry.
        assert_eq!(q.advance_on_end(), Advance::Play(2));
        assert_eq!(q.current_track().unwrap().video_id, "c");
        assert_eq!(q.upcoming_count(), 1);
        // Previous walks back up the same list.
        assert_eq!(q.prev_manual(), Advance::Play(1));
    }

    /// Guards the cover-aspect maths that lives in `App::cover_rows`:
    /// rows = cols * font_width / font_height, rounded.
    #[test]
    fn cover_rows_math_keeps_the_block_square_in_pixels() {
        fn rows(cols: u32, fw: u32, fh: u32) -> u32 {
            ((cols * fw + fh / 2) / fh).max(1)
        }
        // Classic 1:2 cell.
        assert_eq!(rows(34, 10, 20), 17);
        assert_eq!(rows(34, 8, 16), 17);
        // Taller cells need fewer rows, or the art would be stretched.
        assert_eq!(rows(34, 9, 21), 15);
        assert_eq!(rows(20, 7, 15), 9);
        // The resulting block is square within one cell of tolerance.
        for (fw, fh) in [(10, 20), (8, 16), (9, 21), (7, 15), (6, 13)] {
            let cols = 34;
            let r = rows(cols, fw, fh);
            let (px_w, px_h) = (cols * fw, r * fh);
            let err = (px_w as i64 - px_h as i64).abs();
            assert!(
                err <= fh as i64,
                "font {fw}x{fh}: {px_w}x{px_h} is not square (off by {err}px)"
            );
        }
    }

    #[test]
    fn set_context_rejects_bad_input() {
        let mut q = Queue::default();
        assert_eq!(q.set_context(vec![], 0), None);
        assert_eq!(q.set_context(vec![t("a")], 5), None);
        assert!(q.is_empty(), "a rejected context must not touch the queue");
    }

    #[test]
    fn upcoming_count_counts_after_current() {
        let mut q = Queue::default();
        q.append(t("a"));
        q.append(t("b"));
        q.append(t("c"));
        q.jump_to(0);
        assert_eq!(q.upcoming_count(), 2);
        q.jump_to(2);
        assert_eq!(q.upcoming_count(), 0);
    }

    #[test]
    fn shuffle_preserves_history_and_current() {
        let mut q = Queue::default();
        for id in ["a", "b", "c", "d", "e"] {
            q.append(t(id));
        }
        q.jump_to(1);
        q.shuffle_upcoming();
        assert_eq!(q.items()[0].video_id, "a");
        assert_eq!(q.items()[1].video_id, "b");
        assert_eq!(q.current_track().unwrap().video_id, "b");
        let mut rest: Vec<_> = q.items()[2..].iter().map(|x| x.video_id.clone()).collect();
        rest.sort();
        assert_eq!(rest, ["c", "d", "e"]);
    }
}
