//! The per-session work of an annotation recording, beside the controller: the signals the
//! adapters report (pictures, Shift, strokes) in recording time, which pictures to keep, the
//! plan at stop (VAD, pictures, segments), the compose step (pictures and the pasted text) and
//! the bundle folder. The controller owns one `AnnotationCtx` per annotation session and calls
//! into here from its effect handlers; nothing here steps the state machine.
//!
//! Rules kept here: times are recording time with pauses removed (`RecordingClock`); the words
//! and pictures never reach the log (G7); every wait has a timeout (G5); the bundle folder is
//! private (0700 / 0600).

use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use murmur_core::annotations::{
    self as ann, Chunk, InkStroke, PasteTarget, ScreenSample, Selection, Signals, SpeechSpan,
    Summary,
};
use murmur_core::ports::SpeechRegion;
use murmur_core::session::{ComposeWords, TranscriptText};
use murmur_core::{Language, pipeline};

use crate::adapters::draw::{self, FrameInput, Rendered};
use crate::adapters::frontmost::FrontApp;
use crate::adapters::image::{self, Bitmap};
use crate::adapters::pointer::{PointerEvent, PointerFeed};
use crate::adapters::screen::{DisplayInfo, Frame, ScreenCapture};
use crate::adapters::store::AnnotationStatus;
use crate::bundles::{self, DisplayFacts, RecordingClock};
use crate::ipc::InkPoint;

/// The state of one annotation session, from the hotkey to the save.
pub struct AnnotationCtx {
    pub clock: RecordingClock,
    pub created_at_ms: i64,
    pub folder: String,
    pub bundle_dir: PathBuf,
    pub candidates_dir: PathBuf,
    pub screen: Option<ScreenCapture>,
    pub display: Option<DisplayInfo>,
    pub pointer: Option<Box<dyn PointerFeed>>,
    /// Signals in recording time.
    pub signals: Signals,
    /// Frame index of each screen sample (the candidate file name), in sample order.
    pub frame_of_sample: Vec<usize>,
    /// Frame indices written to `candidates/`.
    pub kept: BTreeSet<usize>,
    pub shift_held: bool,
    pub screen_failed: bool,
    /// The app in front at the hotkey: where the bundle is pasted (R1). Taken at delivery.
    pub front: Option<FrontApp>,
    /// Its bundle id, kept for the paste target rule and `annotation.json`.
    pub target_bundle_id: Option<String>,
    pub plan: Option<Planned>,
    pub chunks: Vec<Chunk>,
    pub words: Option<ComposeWords>,
    pub rendered: Option<Rendered>,
    pub compose_ms: Option<u64>,
    /// Why the words failed, when they did (the failed history row names it).
    pub failure: Option<murmur_core::session::FailureKind>,
}

/// The outcome of the plan step.
#[derive(Debug, Clone)]
pub struct Planned {
    pub selection: Selection,
    /// `candidates/<frame>.png` for each picture, in picture order.
    pub pictures: Vec<PathBuf>,
    pub regions: Vec<SpeechRegion>,
    pub total_samples: usize,
}

impl Planned {
    /// Whether the user drew anything: a picture is wanted, even when none could be made.
    pub fn pictures_wanted(&self) -> bool {
        !self.selection.keyframes.is_empty() || self.selection.dropped_groups > 0
    }
}

impl AnnotationCtx {
    /// A fresh context; the bundle folder and `candidates/` are created now (mode 0700).
    pub fn new(
        root: &Path,
        created_at_ms: i64,
        session_id: u64,
        clock: RecordingClock,
    ) -> Result<AnnotationCtx, std::io::Error> {
        let folder = bundles::folder_name(created_at_ms, &format!("{session_id:x}"));
        let bundle_dir = root.join(&folder);
        let candidates_dir = bundle_dir.join("candidates");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(bundles::FOLDER_MODE)
            .create(&candidates_dir)?;
        Ok(AnnotationCtx {
            clock,
            created_at_ms,
            folder,
            bundle_dir,
            candidates_dir,
            screen: None,
            display: None,
            pointer: None,
            signals: Signals::default(),
            frame_of_sample: Vec::new(),
            kept: BTreeSet::new(),
            shift_held: false,
            screen_failed: false,
            front: None,
            target_bundle_id: None,
            plan: None,
            chunks: Vec::new(),
            words: None,
            rendered: None,
            compose_ms: None,
            failure: None,
        })
    }

    /// Pictures the pill shows: the pictures once chosen, else the ink groups so far.
    pub fn images(&self) -> u32 {
        match &self.plan {
            Some(plan) => plan.selection.keyframes.len() as u32,
            None => ann::ink_groups(&self.signals).len() as u32,
        }
    }

    /// A picture arrived. Returns true when the pill should refresh.
    pub fn on_frame(&mut self, frame: Frame) -> bool {
        let Some(t_ms) = self.clock.recording_from_wall(frame.t_ms) else {
            return false;
        };
        self.signals.samples.push(ScreenSample {
            t_ms,
            cells: frame.cells,
        });
        self.frame_of_sample.push(frame.index);
        false
    }

    /// Shift went down or up. Shift down means a stroke may follow: the newest picture is
    /// kept now, so the picture right before the drawing is there even when the stroke takes
    /// longer than the screen adapter's memory. Returns true when the pill should refresh.
    pub fn on_pointer(&mut self, event: PointerEvent) -> bool {
        match event {
            PointerEvent::Shift(held) => {
                self.shift_held = held;
                if held && let Some(frame) = self.frame_of_sample.last().copied() {
                    self.keep(frame);
                }
                false
            }
        }
    }

    /// A stroke from the drawing layer (epoch milliseconds). The picture before its group's
    /// first stroke is kept. Returns true when the pill should refresh.
    pub fn on_stroke(&mut self, points: Vec<InkPoint>, started_ms: f64, ended_ms: f64) -> bool {
        let (Some(t0_ms), Some(t1_ms)) = (
            self.clock.recording_from_epoch(started_ms),
            self.clock.recording_from_epoch(ended_ms),
        ) else {
            return false;
        };
        if points.is_empty() {
            return false;
        }
        let groups_before = ann::ink_groups(&self.signals).len();
        self.signals.strokes.push(InkStroke {
            t0_ms,
            t1_ms: t1_ms.max(t0_ms),
            points: points.into_iter().map(|p| (p.x, p.y)).collect(),
        });
        for sample in ann::ink_samples(&self.signals) {
            if let Some(frame) = self.frame_of_sample.get(sample).copied() {
                self.keep(frame);
            }
        }
        ann::ink_groups(&self.signals).len() != groups_before
    }

    /// Asks the screen adapter to keep a picture as a candidate PNG, once.
    fn keep(&mut self, frame: usize) {
        if self.kept.contains(&frame) {
            return;
        }
        let Some(screen) = &self.screen else {
            return;
        };
        match screen.keep_frame(frame) {
            Ok(()) => {
                self.kept.insert(frame);
            }
            Err(error) => tracing::debug!(%error, "candidate picture not kept"),
        }
    }
}

/// What the controller hands to `plan` (its own copies: the plan runs on another thread).
pub struct PlanInput {
    pub signals: Signals,
    pub frame_of_sample: Vec<usize>,
    pub kept: BTreeSet<usize>,
    pub candidates_dir: PathBuf,
    pub stopped_ms: u64,
}

/// Chooses the pictures and makes sure each has a file on disk. Pure work plus a few PNG
/// writes from the pictures still in memory; blocking, call it off the controller loop.
pub fn plan(
    mut input: PlanInput,
    regions: &[SpeechRegion],
    total_samples: usize,
    recent: &[(usize, Bitmap)],
) -> Planned {
    input.signals.stopped_ms = input.stopped_ms;
    input.signals.speech = regions
        .iter()
        .map(|r| SpeechSpan {
            start_ms: ann::samples_to_ms(r.start),
            end_ms: ann::samples_to_ms(r.end),
        })
        .collect();
    let mut selection = ann::select_keyframes(&input.signals);

    // A file for every picture: kept already, still in memory, or the nearest kept one.
    let mut kept: Vec<usize> = input.kept.iter().copied().collect();
    let mut pictures = Vec::with_capacity(selection.keyframes.len());
    let mut missing = Vec::new();
    for (i, keyframe) in selection.keyframes.iter().enumerate() {
        let frame = input.frame_of_sample.get(keyframe.sample).copied();
        let path_of = |frame: usize| input.candidates_dir.join(format!("{frame}.png"));
        let chosen = match frame {
            Some(frame) if kept.contains(&frame) => Some(frame),
            Some(frame) => match recent.iter().find(|(f, _)| *f == frame) {
                Some((_, bitmap)) => match image::write_png(bitmap, &path_of(frame)) {
                    Ok(()) => {
                        kept.push(frame);
                        Some(frame)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "picture not written");
                        bundles::nearest_kept(frame, &kept)
                    }
                },
                None => bundles::nearest_kept(frame, &kept),
            },
            None => kept.last().copied(),
        };
        match chosen {
            Some(frame) => pictures.push(path_of(frame)),
            None => missing.push(i),
        }
    }
    // A picture without any file cannot be drawn: its groups are dropped and the rest renumbered.
    if !missing.is_empty() {
        tracing::warn!(dropped = missing.len(), "pictures without a file dropped");
        let mut renumber = vec![0usize; selection.keyframes.len() + 1];
        let mut n = 0;
        selection.keyframes = selection
            .keyframes
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !missing.contains(i))
            .map(|(i, mut k)| {
                n += 1;
                renumber[i + 1] = n;
                k.n = n;
                k
            })
            .collect();
        let before = selection.marks.len();
        selection.marks.retain(|m| renumber[m.keyframe] != 0);
        selection.dropped_groups += before - selection.marks.len();
        for (label, mark) in selection.marks.iter_mut().enumerate() {
            mark.keyframe = renumber[mark.keyframe];
            mark.label = label as u32 + 1;
        }
    }
    if selection.dropped_groups > 0 {
        tracing::info!(
            dropped = selection.dropped_groups,
            "ink groups without a picture"
        );
    }
    Planned {
        selection,
        pictures,
        regions: regions.to_vec(),
        total_samples,
    }
}

/// The audio ranges to transcribe, one picture each, and the chunk each becomes.
pub fn segments(planned: &Planned) -> Vec<pipeline::Segment> {
    let starts = ann::keyframe_starts_in_samples(&planned.selection.keyframes);
    pipeline::segment_by_keyframes(
        planned.total_samples,
        &planned.regions,
        &starts,
        ann::segment_lead_samples(),
    )
}

/// The chunk of a transcribed segment (picture index to the 1-based picture number).
pub fn chunk_of(segment: &pipeline::Segment, text: String) -> Chunk {
    Chunk {
        start_ms: ann::samples_to_ms(segment.speech.start),
        end_ms: ann::samples_to_ms(segment.speech.end),
        keyframe: segment.keyframe + 1,
        text: TranscriptText(text),
    }
}

/// What the compose step produced besides the files.
pub struct Composed {
    pub rendered: Rendered,
}

/// Draws the pictures and writes `summary.md` and `annotation.json`. Blocking.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    bundle_dir: &Path,
    planned: &Planned,
    chunks: &[Chunk],
    words: ComposeWords,
    language: Language,
    duration_ms: u64,
    created_at_ms: i64,
    model_id: Option<&str>,
    display: Option<DisplayFacts>,
    target_bundle_id: Option<&str>,
) -> Result<Composed, String> {
    let mut pictures: Vec<Bitmap> = Vec::with_capacity(planned.pictures.len());
    for path in &planned.pictures {
        pictures.push(image::read_png(path).map_err(|e| e.to_string())?);
    }
    // Marks per picture (borrowed slices need the per-picture vectors to live here).
    let per_keyframe: Vec<Vec<ann::Mark>> = planned
        .selection
        .keyframes
        .iter()
        .map(|k| {
            planned
                .selection
                .marks
                .iter()
                .filter(|m| m.keyframe == k.n)
                .cloned()
                .collect()
        })
        .collect();
    let frames: Vec<FrameInput<'_>> = planned
        .selection
        .keyframes
        .iter()
        .zip(&pictures)
        .zip(&per_keyframe)
        .map(|((k, picture), marks)| FrameInput {
            n: k.n,
            picture,
            marks,
        })
        .collect();
    let rendered = draw::render(&frames, bundle_dir).map_err(|e| e.to_string())?;
    let status = status_of(words, true, true);
    // The saved text is the one an ordinary app receives (no paths).
    let summary = summary_text(
        language,
        PasteTarget::Other,
        bundle_dir,
        planned,
        chunks,
        words,
    );
    let json = bundles::annotation_json(
        created_at_ms,
        duration_ms,
        language.code(),
        model_id,
        display,
        target_bundle_id,
        &planned.selection.keyframes,
        &planned.selection.marks,
        chunks,
        status,
    );
    bundles::write_bundle_files(bundle_dir, &summary, &json).map_err(|e| e.to_string())?;
    Ok(Composed { rendered })
}

/// The text to paste: the words with the marks of the pictures (and their paths for a
/// terminal), or the words alone.
pub fn summary_text(
    language: Language,
    target: PasteTarget,
    bundle_dir: &Path,
    planned: &Planned,
    chunks: &[Chunk],
    words: ComposeWords,
) -> String {
    ann::summary_text(&Summary {
        language,
        target,
        bundle_dir: &bundle_dir.to_string_lossy(),
        marks: &planned.selection.marks,
        chunks,
        transcription_failed: words == ComposeWords::Failed,
    })
}

/// The row status: failed words win, then pictures wanted but not made, else ok.
pub fn status_of(words: ComposeWords, pictures_wanted: bool, rendered: bool) -> AnnotationStatus {
    match (words, pictures_wanted, rendered) {
        (ComposeWords::Failed, _, _) => AnnotationStatus::Failed,
        (_, true, false) => AnnotationStatus::TextOnly,
        _ => AnnotationStatus::Ok,
    }
}

/// Facts about the display for `annotation.json`.
pub fn display_facts(display: Option<&DisplayInfo>) -> Option<DisplayFacts> {
    display.map(|d| DisplayFacts {
        width_pt: d.rect.width,
        height_pt: d.rect.height,
        scale: d.scale,
    })
}

/// Removes the candidate pictures after the save (the chosen ones are in `frames/`).
pub fn remove_candidates(candidates_dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(candidates_dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not remove the candidate pictures");
    }
}

/// Removes the whole bundle folder (a dropped session), if it is under `root`.
pub fn remove_bundle(root: &Path, folder: &str) {
    let Some(path) = bundles::bundle_path(root, folder) else {
        return;
    };
    if let Err(error) = std::fs::remove_dir_all(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not remove a dropped bundle folder");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_core::annotations::{GRID_CELLS, Keyframe};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("murmur-plan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(t_ms: u64) -> ScreenSample {
        ScreenSample {
            t_ms,
            cells: vec![0.3; GRID_CELLS],
        }
    }

    fn stroke(t0_ms: u64, t1_ms: u64) -> InkStroke {
        InkStroke {
            t0_ms,
            t1_ms,
            points: vec![(0.1, 0.1), (0.2, 0.2)],
        }
    }

    fn region(start_s: f64, end_s: f64) -> SpeechRegion {
        let sr = murmur_core::SAMPLE_RATE_HZ as f64;
        SpeechRegion {
            start: (start_s * sr) as usize,
            end: (end_s * sr) as usize,
        }
    }

    #[test]
    fn plan_uses_the_kept_picture_before_the_stroke_and_drops_groups_without_one() {
        let dir = scratch();
        let candidates_dir = dir.join("candidates");
        std::fs::create_dir_all(&candidates_dir).unwrap();
        // Frames 0..=5 at 500 ms steps; only frame 3 (1.5 s) was kept on disk.
        std::fs::write(candidates_dir.join("3.png"), b"png").unwrap();
        let signals = Signals {
            stopped_ms: 4_000,
            samples: (0..6).map(|i| sample(i * 500)).collect(),
            strokes: vec![stroke(2_100, 2_400)],
            speech: Vec::new(),
        };
        let input = PlanInput {
            signals,
            frame_of_sample: (0..6).collect(),
            kept: [3].into_iter().collect(),
            candidates_dir: candidates_dir.clone(),
            stopped_ms: 4_000,
        };
        let planned = plan(input, &[region(0.0, 3.0)], 64_000, &[]);
        assert_eq!(planned.selection.keyframes.len(), 1);
        // The sample before 2.1 s is frame 4 (2.0 s); it is not kept, the nearest kept is 3.
        assert_eq!(planned.pictures, vec![candidates_dir.join("3.png")]);
        assert_eq!(planned.selection.marks.len(), 1);
        assert!(planned.pictures_wanted());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plan_without_any_kept_picture_drops_the_group_and_still_wants_pictures() {
        let dir = scratch();
        let candidates_dir = dir.join("candidates");
        let signals = Signals {
            stopped_ms: 4_000,
            samples: (0..6).map(|i| sample(i * 500)).collect(),
            strokes: vec![stroke(2_100, 2_400)],
            speech: Vec::new(),
        };
        let input = PlanInput {
            signals,
            frame_of_sample: (0..6).collect(),
            kept: BTreeSet::new(),
            candidates_dir,
            stopped_ms: 4_000,
        };
        let planned = plan(input, &[], 64_000, &[]);
        assert!(planned.selection.keyframes.is_empty());
        assert!(planned.selection.marks.is_empty());
        assert_eq!(planned.selection.dropped_groups, 1);
        assert!(planned.pictures_wanted());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn words_before_the_first_picture_are_in_the_first_segment() {
        let planned = Planned {
            selection: Selection {
                keyframes: vec![Keyframe {
                    n: 1,
                    t_ms: 10_000,
                    sample: 20,
                }],
                marks: Vec::new(),
                dropped_groups: 0,
            },
            pictures: vec![PathBuf::from("x")],
            regions: vec![region(0.5, 5.0), region(11.0, 14.0)],
            total_samples: 16_000 * 20,
        };
        // Both regions belong to the only picture, so they pack into one segment.
        let segments = segments(&planned);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].keyframe, 0);
        let chunk = chunk_of(&segments[0], "words".into());
        assert_eq!(
            (chunk.start_ms, chunk.end_ms, chunk.keyframe),
            (500, 14_000, 1)
        );
    }

    #[test]
    fn status_rules() {
        assert_eq!(
            status_of(ComposeWords::Failed, true, true),
            AnnotationStatus::Failed
        );
        assert_eq!(
            status_of(ComposeWords::Transcribed, true, false),
            AnnotationStatus::TextOnly
        );
        assert_eq!(
            status_of(ComposeWords::Transcribed, false, false),
            AnnotationStatus::Ok
        );
        assert_eq!(
            status_of(ComposeWords::None, true, true),
            AnnotationStatus::Ok
        );
    }

    #[test]
    fn no_plan_means_no_pictures_wanted() {
        let planned = Planned {
            selection: Selection::default(),
            pictures: Vec::new(),
            regions: Vec::new(),
            total_samples: 0,
        };
        assert!(!planned.pictures_wanted());
    }
}
