// Copyright 2016 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Compute line wrapping breaks for text.

use time;

use xi_rope::rope::{Rope, RopeInfo};
use xi_rope::tree::Cursor;
use xi_rope::interval::Interval;
use xi_rope::breaks::{Breaks, BreakBuilder, BreaksBaseMetric};
use xi_rope::spans::Spans;
use xi_unicode::LineBreakLeafIter;

use styles::Style;
use tabs::DocumentCtx;
use width_cache::{Token, WidthCache};

struct LineBreakCursor<'a> {
    inner: Cursor<'a, RopeInfo>,
    lb_iter: LineBreakLeafIter,
    last_byte: u8,
}

impl<'a> LineBreakCursor<'a> {
    fn new(text: &'a Rope, pos: usize) -> LineBreakCursor<'a> {
        let inner = Cursor::new(text, pos);
        let lb_iter = match inner.get_leaf() {
            Some((s, offset)) if !s.is_empty() =>
                LineBreakLeafIter::new(s.as_str(), offset),
            _ => LineBreakLeafIter::default()
        };
        LineBreakCursor {
            inner: inner,
            lb_iter: lb_iter,
            last_byte: 0,
        }
    }

    // position and whether break is hard; up to caller to stop calling after EOT
    fn next(&mut self) -> (usize, bool) {
        let mut leaf = self.inner.get_leaf();
        loop {
            match leaf {
                Some((s, offset)) => {
                    let (next, hard) = self.lb_iter.next(s.as_str());
                    if next < s.len() {
                        return (self.inner.pos() - offset + next, hard);
                    }
                    if !s.is_empty() {
                        self.last_byte = s.as_bytes()[s.len() - 1];
                    }
                    leaf = self.inner.next_leaf();
                }
                // A little hacky but only reports last break as hard if final newline
                None => return (self.inner.pos(), self.last_byte == b'\n')
            }
        }
    }
}

pub fn linewrap(text: &Rope, cols: usize) -> Breaks {
    let start_time = time::now();
    let mut lb_cursor = LineBreakCursor::new(text, 0);
    let mut builder = BreakBuilder::new();
    let mut last_pos = 0;
    let mut last_break_pos = 0;
    let mut width = 0;
    loop {
        let (pos, hard) = lb_cursor.next();
        let word_width = pos - last_pos;
        if width > 0 && width + word_width > cols {
            builder.add_break(width);
            //eprintln!("soft break {}", width);
            last_break_pos += width;
            width = 0;
        }
        width += word_width;
        if hard {
            builder.add_break(width);
            //eprintln!("hard break {}", width);
            last_break_pos += width;
            width = 0;
        }
        last_pos = pos;
        if pos == text.len() { break; }
    }
    builder.add_no_break(text.len() - last_break_pos);
    let result = builder.build();
    let time_ms = (time::now() - start_time).num_nanoseconds().unwrap() as f64 * 1e-6;
    eprintln!("time to wrap {} bytes: {:.2}ms", text.len(), time_ms);
    result
}

// `text` is string _after_ editing.
pub fn rewrap(breaks: &mut Breaks, text: &Rope, iv: Interval, newsize: usize, cols: usize) {
    let (edit_iv, new_breaks) = {
        let start_time = time::now();
        let (start, end) = iv.start_end();
        let mut bk_cursor = Cursor::new(breaks, start);
        // start of range to invalidate
        let mut inval_start = bk_cursor.prev::<BreaksBaseMetric>().unwrap_or(0);
        if inval_start > 0 {
            // edit on this line can invalidate break at end of previous
            inval_start = bk_cursor.prev::<BreaksBaseMetric>().unwrap_or(0);
        }
        bk_cursor.set(end);
        // compute end position in edited rope
        let mut inval_end = bk_cursor.next::<BreaksBaseMetric>().map_or(text.len(), |pos|
            pos - (end - start) + newsize);
        let mut lb_cursor = LineBreakCursor::new(text, inval_start);
        let mut builder = BreakBuilder::new();
        let mut last_pos = inval_start;
        let mut last_break_pos = inval_start;
        let mut width = 0;
        loop {
            let (pos, hard) = lb_cursor.next();
            let word_width = pos - last_pos;
            if width > 0 && width + word_width > cols {
                builder.add_break(width);
                last_break_pos += width;
                width = 0;
                while last_break_pos > inval_end {
                    inval_end = bk_cursor.next::<BreaksBaseMetric>().map_or(text.len(), |pos|
                        pos - (end - start) + newsize);
                }
                if last_break_pos == inval_end {
                    break;
                }
            }
            width += word_width;
            if hard {
                // TODO: DRY
                builder.add_break(width);
                last_break_pos += width;
                width = 0;
                while last_break_pos > inval_end {
                    inval_end = bk_cursor.next::<BreaksBaseMetric>().map_or(text.len(), |pos|
                        pos - (end - start) + newsize);
                }
                if last_break_pos == inval_end {
                    break;
                }
            }
            last_pos = pos;
            if pos == text.len() {
                break;
            }
        }
        builder.add_no_break(inval_end - last_break_pos);
        let time_ms = (time::now() - start_time).num_nanoseconds().unwrap() as f64 * 1e-6;
        eprintln!("time to wrap {} bytes: {:.2}ms (not counting build+edit)",
            inval_end - inval_start, time_ms);
        (Interval::new_open_closed(inval_start, inval_end + (end - start) - newsize), builder.build())
    };
    breaks.edit(edit_iv, new_breaks);
}

/// A potential opportunity to insert a break. In this representation, the widths
/// have been requested (in a batch request) but are not necessarily known until
/// the request is issued.
struct PotentialBreak {
    pos: usize,  // offset within text of the end of the word
    tok: Token,
    hard: bool,
}

// State for a rewrap in progress
struct RewrapCtx<'a> {
    text: &'a Rope,
    lb_cursor: LineBreakCursor<'a>,
    lb_cursor_pos: usize,
    width_cache: &'a mut WidthCache,
    doc_ctx: &'a DocumentCtx,
    pot_breaks: Vec<PotentialBreak>,
    pot_break_ix: usize,  // index within pot_breaks
    max_offset: usize, // offset of maximum break (ie hard break following edit)
    max_width: f64,
}

// This constant should be tuned so that the RPC takes about 1ms. Less than that,
// RPC overhead becomes significant. More than that, interactivity suffers.
const MAX_POT_BREAKS: usize = 10_000;

impl<'a> RewrapCtx<'a> {
    fn new(text: &'a Rope, _style_spans: &Spans<Style>, doc_ctx: &'a DocumentCtx,
        max_width: f64, width_cache: &'a mut WidthCache) -> RewrapCtx<'a>
    {
        let lb_cursor_pos = 0;
        let lb_cursor = LineBreakCursor::new(text, 0);
        RewrapCtx {
            text,
            lb_cursor,
            lb_cursor_pos,
            width_cache,
            doc_ctx,
            pot_breaks: Vec::new(),
            pot_break_ix: 0,
            max_offset: text.len(),
            max_width,
        }
    }

    fn refill_pot_breaks(&mut self) {
        let style_id = 2;  // TODO: derive from style spans rather than assuming.

        let mut req = self.width_cache.batch_req();

        self.pot_breaks.clear();
        self.pot_break_ix = 0;
        let mut pos = self.lb_cursor_pos;
        while pos < self.max_offset && self.pot_breaks.len() < MAX_POT_BREAKS {
            let (next, hard) = self.lb_cursor.next();
            // TODO: avoid allocating string
            let word = self.text.slice_to_string(pos, next);
            let tok = req.request(style_id, &word);
            pos = next;
            self.pot_breaks.push(PotentialBreak { pos, tok, hard });
        }
        req.issue(self.doc_ctx).unwrap();
        self.lb_cursor_pos = pos;
    }

    // Compute the next break, assuming `start` is a valid break.
    //
    // Invariant: start corresponds to the start of the word referenced by pot_break_ix.
    fn wrap_one_line(&mut self, start: usize) -> Option<usize> {
        let mut line_width = 0.0;
        let mut pos = start;
        while pos < self.max_offset {
            if self.pot_break_ix >= self.pot_breaks.len() {
                self.refill_pot_breaks();
            }
            let pot_break = &self.pot_breaks[self.pot_break_ix];
            if pot_break.hard {
                self.pot_break_ix += 1;
                return Some(pot_break.pos);
            }
            let width = self.width_cache.resolve(pot_break.tok);
            if line_width == 0.0 && width >= self.max_width {
                self.pot_break_ix += 1;
                return Some(pot_break.pos);
            }
            line_width += width;
            if line_width > self.max_width {
                return Some(pos);
            }
            self.pot_break_ix += 1;
            pos = pot_break.pos;
        }
        None
    }
}

/// Wrap the text (in batch mode) using width measurement.
pub fn linewrap_width(text: &Rope, style_spans: &Spans<Style>, doc_ctx: &DocumentCtx,
        max_width: f64) -> Breaks
{
    // TODO: this should be scoped to the FE. However, it will work (just with
    // degraded performance).
    let mut width_cache = WidthCache::new();
    let mut ctx = RewrapCtx::new(text, style_spans, doc_ctx, max_width, &mut width_cache);
    let mut builder = BreakBuilder::new();
    let mut pos = 0;
    while let Some(next) = ctx.wrap_one_line(pos) {
        builder.add_break(next - pos);
        pos = next;
    }
    builder.add_no_break(text.len() - pos);
    builder.build()
}
