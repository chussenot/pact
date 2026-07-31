//! The `pact ui` mascot: a small ASCII creature that reacts to what you do.
//!
//! Art, timings and the gesture-per-event mapping live in
//! `docs/mascot-animations.md`; this file is the transcription plus the state
//! machine. `src/tui.rs` decides *when* to `play` a gesture; nothing here knows
//! about the UI.
//!
//! Frame tables were generated mechanically from the doc's fenced blocks, so the
//! blank rows (`""`) and leading spaces are exactly the animator's — they are the
//! creature's vertical and horizontal position inside the fixed box, not padding.
//! Raw strings are used because the art contains backslashes and no quotes.

use std::time::{Duration, Instant};

/// Fixed art box so the layout never jitters between frames.
pub const ART_WIDTH: u16 = 16;
pub const ART_HEIGHT: u16 = 5;

/// One frame: how long it stays on screen (ms) and its `ART_HEIGHT` lines.
type Frame = (u64, [&'static str; ART_HEIGHT as usize]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    Idle,
    Jump,
    Wave,
    Cheer,
    Peek,
    Flex,
    Shrug,
    Alarmed,
}

impl Gesture {
    fn frames(self) -> &'static [Frame] {
        match self {
            Gesture::Idle => &IDLE,
            Gesture::Jump => &JUMP,
            Gesture::Wave => &WAVE,
            Gesture::Cheer => &CHEER,
            Gesture::Peek => &PEEK,
            Gesture::Flex => &FLEX,
            Gesture::Shrug => &SHRUG,
            Gesture::Alarmed => &ALARMED,
        }
    }

    /// Idle breathes forever; Alarmed pulses for as long as the state it mirrors
    /// lasts (`tui.rs` ends it with an explicit `play(Idle)`). Everything else is
    /// a one-shot that falls back to Idle.
    fn loops(self) -> bool {
        matches!(self, Gesture::Idle | Gesture::Alarmed)
    }
}

pub struct Mascot {
    gesture: Gesture,
    index: usize,
    /// When the current frame became visible.
    frame_start: Instant,
}

/// Catch-up bound for `tick`: after a suspend the clock can be hours ahead, and
/// walking it 70 ms at a time is pointless. Past this we resync to `now`.
const MAX_CATCHUP: usize = 64;

impl Mascot {
    /// Fresh mascot, already looping its Idle animation.
    pub fn new(now: Instant) -> Self {
        Mascot {
            gesture: Gesture::Idle,
            index: 0,
            frame_start: now,
        }
    }

    /// Start playing gesture from its first frame. A one-shot gesture
    /// (loops == false) plays through then falls back to looping Idle.
    /// Calling play with the same gesture already playing restarts it.
    pub fn play(&mut self, gesture: Gesture, now: Instant) {
        self.gesture = gesture;
        self.index = 0;
        self.frame_start = now;
    }

    /// Advance the animation clock to now. Returns true if the visible frame
    /// changed, i.e. the UI needs a redraw.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = *self.art();
        for _ in 0..MAX_CATCHUP {
            let due = self.frame_start + self.frame_duration();
            if now < due {
                return before != *self.art();
            }
            self.frame_start = due;
            self.advance();
        }
        // Still behind after MAX_CATCHUP frames: the process was suspended.
        // Resync instead of grinding through the backlog.
        self.frame_start = now;
        before != *self.art()
    }

    /// How long until the next frame is due. The event loop uses this to size
    /// its poll timeout.
    pub fn next_frame_in(&self, now: Instant) -> Option<Duration> {
        let left = (self.frame_start + self.frame_duration()).saturating_duration_since(now);
        // Never hand the event loop a zero timeout: that is a busy spin. A frame
        // already overdue is due "in 1 ms" and the next tick will advance it.
        Some(left.max(Duration::from_millis(1)))
    }

    /// Current frame art: exactly ART_HEIGHT lines, each at most ART_WIDTH
    /// display columns wide.
    pub fn frame(&self) -> &[&str] {
        self.art()
    }

    /// Gesture currently playing (for tests and the status line).
    pub fn gesture(&self) -> Gesture {
        self.gesture
    }

    fn art(&self) -> &'static [&'static str; ART_HEIGHT as usize] {
        &self.gesture.frames()[self.index].1
    }

    fn frame_duration(&self) -> Duration {
        Duration::from_millis(self.gesture.frames()[self.index].0)
    }

    /// Step to the next frame, looping or falling back to Idle at the end.
    fn advance(&mut self) {
        let next = self.index + 1;
        if next < self.gesture.frames().len() {
            self.index = next;
        } else if self.gesture.loops() {
            self.index = 0;
        } else {
            // Idle frame 0 is the exhale, so the handoff reads as one last
            // breath out rather than a pop. See docs/mascot-animations.md.
            self.gesture = Gesture::Idle;
            self.index = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Frame tables — transcribed from docs/mascot-animations.md.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const IDLE: [Frame; 4] = [
    (800, ["", "", r"   .-----.", r"  -|o - o|-", r"   '-^-^-'"]),
    (800, ["", r"   .-----.", r"   |o   o|", r"  -|  -  |-", r"   '-^-^-'"]),
    (160, ["", r"   .-----.", r"   |-   -|", r"  -|  -  |-", r"   '-^-^-'"]),
    (500, ["", r"   .-----.", r"   |o   o|", r"  -|  -  |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const JUMP: [Frame; 7] = [
    (90, ["", "", "", r"  .-------.", r" -|^  -  ^|-"]),
    (70, [r"    .---.", r"    |o o|", r"    | O |", r"   -|   |-", r"    '-^-'"]),
    (90, [r"    .---.", r"   \|o o|/", r"    | O |", r"    '-v-'", r"    -----"]),
    (130, [r"   .-----.", r"  \|^   ^|/", r"   | \_/ |", r"   '--v--'", r"     ---"]),
    (80, ["", r"    .---.", r"   \|o o|/", r"    | - |", r"    /   \"]),
    (110, ["", "", "", r"  .-------.", r" -|-  _  -|-"]),
    (120, ["", "", r"  .-------.", r" -| o - o |-", r"  '--^-^--'"]),
];

#[rustfmt::skip]
const WAVE: [Frame; 5] = [
    (120, ["", r"   .-----.", r"   |o   o|/", r"  -| \_/ |", r"   '-^-^-'"]),
    (100, ["", r"   .-----./", r"   |^   ^|", r"  -| \_/ |", r"   '-^-^-'"]),
    (100, ["", r"   .-----.", r"   |^   ^|/", r"  -| \_/ |", r"   '-^-^-'"]),
    (110, ["", r"   .-----./", r"   |^   ^|", r"  -| \_/ |", r"   '-^-^-'"]),
    (150, ["", r"   .-----.", r"   |o   o|", r"  -| \_/ |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const CHEER: [Frame; 5] = [
    (90, ["", "", r"  .-------.", r" -| ^ - ^ |-", r"  '--^-^--'"]),
    (120, [r"   \.---./", r"    |o o|", r"    | O |", r"    '-v-'", r"    -----"]),
    (200, [r" . \.---./ .", r"    |^ ^|", r"    | O |", r"    '-v-'", r"    -----"]),
    (110, ["", r"   \.---./", r"    |^ ^|", r"    | - |", r"    /   \"]),
    (160, ["", r"   .-----.", r"   |^   ^|", r"  -| \_/ |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const PEEK: [Frame; 5] = [
    (120, ["", r"   .-----.", r"   | o  o|", r"  -| \_/ |", r"   '-^-^-'"]),
    (130, ["", r"     .-----.", r"    -|  o o|", r"     |  o  |", r"     '-^-^-'"]),
    (260, ["", r"     .--^--.", r"    -|  O o|", r"     |  o  |", r"     '-^-^-'"]),
    (130, ["", r"   .-----.", r"   |o   o|", r"  -|  o  |-", r"   '-^-^-'"]),
    (150, ["", r"   .-----.", r"   |o   o|", r"  -|  -  |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const FLEX: [Frame; 5] = [
    (130, ["", "", r"   .-----.", r"  -|^ - ^|-", r"   '-^-^-'"]),
    (110, ["", r"  .-------.", r" <|o     o|>", r"  |  \_/  |", r"  '--^-^--'"]),
    (
        300,
        [
            r"            *",
            r"  .-------.",
            r" <|^     ^|>",
            r"  |  \_/  |",
            r"  '--^-^--'",
        ],
    ),
    (130, ["", r"   .-----.", r"   |^   ^|", r"  -| \_/ |-", r"   '-^-^-'"]),
    (160, ["", r"   .-----.", r"   |o   o|", r"  -|  -  |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const SHRUG: [Frame; 4] = [
    (140, ["", r"   .-----.", r"   |o   o|", r"  -|  ~  |-", r"   '-^-^-'"]),
    (160, ["", r"   .-----.", r"  _|o   o|_", r"   |  ~  |", r"   '-^-^-'"]),
    (380, ["", "", r"  .-------.", r" _| o ~ o |_", r"  '--^-^--'"]),
    (200, ["", r"   .-----.", r"   |o   o|", r"  -|  -  |-", r"   '-^-^-'"]),
];

#[rustfmt::skip]
const ALARMED: [Frame; 3] = [
    (160, [r"       !", r"   .-----.", r"  \|O   O|/", r"   |  o  |", r"   '-^-^-'"]),
    (160, ["", r"    .-----.", r"   \|o   o|/", r"    |  o  |", r"    '-^-^-'"]),
    (160, [r"       !", r"   .-----. ,", r"  \|O   O|/", r"   |  o  |", r"   '-^-^-'"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Gesture; 8] = [
        Gesture::Idle,
        Gesture::Jump,
        Gesture::Wave,
        Gesture::Cheer,
        Gesture::Peek,
        Gesture::Flex,
        Gesture::Shrug,
        Gesture::Alarmed,
    ];

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn total_ms(g: Gesture) -> u64 {
        g.frames().iter().map(|f| f.0).sum()
    }

    /// The layout guarantee: no art edit may ever make a frame taller than
    /// ART_HEIGHT, wider than ART_WIDTH, or non-ASCII (wide chars break the
    /// fixed box). Asserted over every frame of every gesture.
    #[test]
    fn every_frame_fits_the_art_box() {
        for g in ALL {
            let frames = g.frames();
            assert!(!frames.is_empty(), "{g:?} has no frames");
            for (i, (dur, art)) in frames.iter().enumerate() {
                assert_eq!(art.len(), ART_HEIGHT as usize, "{g:?} frame {i} height");
                for line in art {
                    assert!(
                        line.chars().count() <= ART_WIDTH as usize,
                        "{g:?} frame {i} line too wide: {line:?}"
                    );
                    assert!(
                        line.is_ascii(),
                        "{g:?} frame {i} is not plain ASCII: {line:?}"
                    );
                    assert_eq!(*line, line.trim_end(), "{g:?} frame {i} trailing space");
                }
                assert!(*dur > 0, "{g:?} frame {i} has zero duration");
            }
        }
    }

    /// Frame counts and loop flags as specified in docs/mascot-animations.md.
    #[test]
    fn frame_counts_and_loop_flags_match_the_spec() {
        let spec = [
            (Gesture::Idle, 4, true),
            (Gesture::Jump, 7, false),
            (Gesture::Wave, 5, false),
            (Gesture::Cheer, 5, false),
            (Gesture::Peek, 5, false),
            (Gesture::Flex, 5, false),
            (Gesture::Shrug, 4, false),
            (Gesture::Alarmed, 3, true),
        ];
        for (g, count, loops) in spec {
            assert_eq!(g.frames().len(), count, "{g:?} frame count");
            assert_eq!(g.loops(), loops, "{g:?} loop flag");
        }
        let total: usize = ALL.iter().map(|g| g.frames().len()).sum();
        assert_eq!(total, 38, "38 frames in the sheet");
    }

    #[test]
    fn new_starts_on_idle_frame_zero() {
        let t = Instant::now();
        let m = Mascot::new(t);
        assert_eq!(m.gesture(), Gesture::Idle);
        assert_eq!(m.frame(), IDLE[0].1);
    }

    #[test]
    fn tick_advances_on_the_frame_schedule_and_reports_changes() {
        let t = Instant::now();
        let mut m = Mascot::new(t);
        m.play(Gesture::Wave, t); // 120 / 100 / 100 / 110 / 150

        // Nothing due yet: no change, no redraw.
        assert!(!m.tick(t + ms(119)));
        assert_eq!(m.frame(), WAVE[0].1);

        // Exactly on the boundary the frame flips.
        assert!(m.tick(t + ms(120)));
        assert_eq!(m.frame(), WAVE[1].1);

        // A second tick inside the same frame is not a redraw.
        assert!(!m.tick(t + ms(200)));
        assert_eq!(m.frame(), WAVE[1].1);

        assert!(m.tick(t + ms(220)));
        assert_eq!(m.frame(), WAVE[2].1);

        // A single tick may cross several frames at once.
        assert!(m.tick(t + ms(430)));
        assert_eq!(m.frame(), WAVE[4].1);
        assert_eq!(m.gesture(), Gesture::Wave);
    }

    #[test]
    fn one_shot_gestures_fall_back_to_looping_idle() {
        for g in ALL.iter().copied().filter(|g| !g.loops()) {
            let t = Instant::now();
            let mut m = Mascot::new(t);
            m.play(g, t);

            let total = total_ms(g);
            // Still playing one tick before the last frame expires.
            assert!(m.tick(t + ms(total - 1)));
            assert_eq!(m.gesture(), g, "{g:?} ended early");

            // Last frame expires -> Idle, on the exhale frame.
            assert!(m.tick(t + ms(total)));
            assert_eq!(m.gesture(), Gesture::Idle, "{g:?} did not fall back");
            assert_eq!(m.frame(), IDLE[0].1, "{g:?} fell back to the wrong frame");

            // And Idle keeps breathing from there.
            m.tick(t + ms(total + 800));
            assert_eq!(m.gesture(), Gesture::Idle);
            assert_eq!(m.frame(), IDLE[1].1);
        }
    }

    #[test]
    fn looping_gestures_never_leave_themselves() {
        for g in ALL.iter().copied().filter(|g| g.loops()) {
            let t = Instant::now();
            let mut m = Mascot::new(t);
            m.play(g, t);
            let cycle = total_ms(g);
            for n in 1..=100 {
                m.tick(t + ms(cycle * n));
                assert_eq!(m.gesture(), g, "{g:?} left itself after {n} cycles");
                assert_eq!(m.frame(), g.frames()[0].1, "{g:?} cycle {n} misaligned");
            }
        }
    }

    #[test]
    fn play_restarts_the_same_gesture_from_frame_zero() {
        let t = Instant::now();
        let mut m = Mascot::new(t);
        m.play(Gesture::Peek, t);
        m.tick(t + ms(300));
        assert_ne!(m.frame(), PEEK[0].1);

        m.play(Gesture::Peek, t + ms(300));
        assert_eq!(m.frame(), PEEK[0].1);
        assert!(!m.tick(t + ms(400)));
        assert_eq!(m.frame(), PEEK[0].1, "restart reset the clock too");
    }

    /// Requirement: the event loop must never get a zero timeout, or it spins at
    /// 100% CPU. Walk every gesture frame by frame and also poll an overdue
    /// mascot without ticking it.
    #[test]
    fn next_frame_in_is_always_some_and_never_zero() {
        let t = Instant::now();
        for g in ALL {
            let mut m = Mascot::new(t);
            m.play(g, t);
            for step in 0..600u64 {
                let now = t + ms(step * 37);
                m.tick(now);
                let left = m.next_frame_in(now).expect("always Some");
                assert!(left >= ms(1), "{g:?} step {step} returned {left:?}");
                assert!(left <= ms(800), "{g:?} step {step} returned {left:?}");
            }
        }

        // Overdue and never ticked: still a positive, bounded timeout.
        let mut m = Mascot::new(t);
        m.play(Gesture::Flex, t);
        let left = m.next_frame_in(t + Duration::from_secs(3600)).unwrap();
        assert_eq!(left, ms(1));
    }

    /// A long suspend must not walk thousands of frames, and must leave the
    /// mascot in a sane, still-animating state.
    #[test]
    fn a_long_suspend_resyncs_instead_of_grinding() {
        let t = Instant::now();
        let mut m = Mascot::new(t);
        m.play(Gesture::Alarmed, t);
        m.tick(t + Duration::from_secs(3600));
        assert_eq!(m.gesture(), Gesture::Alarmed);
        let after = t + Duration::from_secs(3600);
        assert!(m.next_frame_in(after).unwrap() >= ms(1));
        assert!(m.tick(after + ms(160)));
    }
}
