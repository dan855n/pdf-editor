//! Full-document search (debounced, cancellable).

use super::*;

impl EditorApp {
    // ------------------------------------------------------------- search

    /// Debounce a query the user just typed: record it and re-check after a
    /// short delay instead of spawning a scan per keystroke.
    pub(super) fn debounce_search(&mut self, query: String) {
        self.last_searched = query.clone();
        if query.is_empty() {
            self.clear_search();
            return;
        }
        // Invalidate any in-flight search immediately so its result cannot
        // land while the debounced query is still pending.
        self.search_gen = self.search_gen.wrapping_add(1);
        self.search_pending_query = Some(query);
        self.search_debounce_deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
    }

    /// Run a debounced query once its deadline has passed.
    pub(super) fn tick_search(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.search_debounce_deadline else {
            return;
        };
        let now = std::time::Instant::now();
        if now < deadline {
            ctx.request_repaint_after(deadline - now);
            return;
        }
        self.search_debounce_deadline = None;
        if let Some(query) = self.search_pending_query.take()
            && !query.is_empty()
        {
            self.start_search(&query);
        }
    }

    /// Kick off a background search of every page for the given query.
    pub(super) fn start_search(&mut self, query: &str) {
        // Any direct start cancels a pending debounce.
        self.search_pending_query = None;
        self.search_debounce_deadline = None;

        let query = query.trim().to_owned();
        if query.is_empty() {
            self.search_hits.clear();
            self.pending_hit_scroll = None;
            self.search_rx = None;
            return;
        }

        // Refresh the read-only render docs first so search sees the latest
        // edits (original-text changes) rather than the file as it was opened.
        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }

        // Bump the generation so any in-flight (now stale) search is discarded.
        self.search_gen = self.search_gen.wrapping_add(1);
        let generation = self.search_gen;
        self.search_hits.clear();
        self.pending_hit_scroll = None;

        let Some(doc) = self.document.as_ref() else {
            return;
        };
        // Use a read-only render handle for the worker thread.
        let doc_arc = match doc.render_docs.first() {
            Some(d) => Arc::clone(d),
            None => {
                self.search_rx = None;
                return;
            }
        };

        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);
        std::thread::spawn(move || {
            let hits = search_all_pages(&doc_arc, &query);
            let _ = tx.send((generation, hits));
        });
    }

    /// Apply the result of a finished background search, unless it's stale.
    pub(super) fn poll_search(&mut self) {
        let mut ready: Option<(u64, Vec<SearchHit>)> = None;
        if let Some(rx) = &self.search_rx {
            match rx.try_recv() {
                Ok(v) => ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.search_rx = None,
            }
        }
        if let Some((generation, hits)) = ready {
            self.search_rx = None;
            if generation == self.search_gen {
                self.search_hits = hits;
                self.search_selected = 0;
                if !self.search_hits.is_empty() {
                    self.pending_hit_scroll = Some(0);
                }
            }
        }
    }

    pub(super) fn goto_hit(&mut self, delta: isize) {
        if self.search_hits.is_empty() {
            return;
        }
        let n = self.search_hits.len() as isize;
        let next = (self.search_selected as isize + delta).rem_euclid(n);
        self.search_selected = next as usize;
        self.pending_hit_scroll = Some(next as usize);
    }

    pub(super) fn clear_search(&mut self) {
        self.search_hits.clear();
        self.search_query.clear();
        self.last_searched.clear();
        self.pending_hit_scroll = None;
        self.search_pending_query = None;
        self.search_debounce_deadline = None;
        // Invalidate any in-flight search so a stale result can't reappear.
        self.search_gen = self.search_gen.wrapping_add(1);
        self.search_rx = None;
    }

}
