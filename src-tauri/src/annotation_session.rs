//! The per-session work of an annotation recording, beside the controller: the signals the
//! adapters report (pictures, clicks, strokes, cursor, app names) in recording time, which
//! candidate pictures to keep, the plan at stop (VAD, keyframes, segments), the compose step
//! (pictures and summary) and the bundle folder. The controller owns one `AnnotationCtx` per
//! annotation session and calls into here from its effect handlers; nothing here steps the
//! state machine.
//!
//! Rules kept here: times are recording time with pauses removed (`RecordingClock`); the words,
//! app names and pictures never reach the log (G7); every wait has a timeout (G5); the bundle
//! folder is private (0700 / 0600).

use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use murmur_core::annotations::{
    self as ann, AppName, Chunk, Click, CursorSample, InkStroke, PasteTarget, ScreenSample,
    Selection, Signals, SpeechSpan, Summary,
};
use murmur_core::ports::SpeechRegion;
use murmur_core::session::{ComposeWords, TranscriptText};
use murmur_core::{Language, pipeline};

use crate::adapters::image::{self, Bitmap};
use crate::adapters::pointer::{PointerEvent, PointerFeed};
use crate::adapters::screen::{DisplayInfo, Frame, ScreenCapture};
use crate::adapters::sheet::{self, FrameInput, Rendered};
use crate::adapters::store::AnnotationStatus;
use crate::bundles::{self, DisplayFacts, RecordingClock};
use crate::ipc::InkPoint;

/// What the live recording has gathered, plus what the stop pipeline adds.
pub struct AnnotationCtx {
    pub clock: RecordingClock,
    pub created_at_ms: i64,
    pub folder: String,
    pub bundle_dir: PathBuf,
    pub candidates_dir: PathBuf,
    pub screen: Option<ScreenCapture>,
    pub display: Option<DisplayInfo>,
    pub pointer: Option<Box<dyn PointerFeed>>,
    /// Signals in recording time. App names are filled in at plan time from `apps`.
    pub signals: Signals,
    /// Frame index of each screen sample (the candidate file name), in sample order.
    pub frame_of_sample: Vec<usize>,
    /// App name changes in recording time.
    pub apps: Vec<(u64, String)>,
    /// Frame indices written to `candidates/`.
    pub kept: BTreeSet<usize>,
    pub shift_held: bool,
    pub screen_failed: bool,
    /// The app in front when the recording stopped (where the bundle is pasted).
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
    /// `candidates/<frame>.png` for each keyframe, in keyframe order.
    pub pictures: Vec<PathBuf>,
    pub regions: Vec<SpeechRegion>,
    pub total_samples: usize,
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
            apps: Vec::new(),
            kept: BTreeSet::new(),
            shift_held: false,
            screen_failed: false,
            target_bundle_id: None,
            plan: None,
            chunks: Vec::new(),
            words: None,
            rendered: None,
            compose_ms: None,
            failure: None,
        })
    }

    /// Pictures the pill shows: the keyframes once chosen, else the candidates kept so far.
    pub fn images(&self) -> u32 {
        match &self.plan {
            Some(plan) => plan.selection.keyframes.len() as u32,
            None => self.kept.len() as u32,
        }
    }

    /// A picture arrived. Returns true when the kept set changed (the pill shows the count).
    pub fn on_frame(&mut self, frame: Frame) -> bool {
        let Some(t_ms) = self.clock.recording_from_wall(frame.t_ms) else {
            return false;
        };
        self.signals.samples.push(ScreenSample {
            t_ms,
            cells: frame.cells,
            app: AppName::default(),
        });
        self.frame_of_sample.push(frame.index);
        self.keep_candidates()
    }

    pub fn on_pointer(&mut self, event: PointerEvent) -> bool {
        match event {
            PointerEvent::Click { t_ms, at } => {
                let Some(t_ms) = self.clock.recording_from_wall(t_ms) else {
                    return false;
                };
                self.signals.clicks.push(Click { t_ms, at });
                self.keep_candidates()
            }
            PointerEvent::Cursor { t_ms, at } => {
                if let Some(t_ms) = self.clock.recording_from_wall(t_ms) {
                    self.signals.cursor.push(CursorSample { t_ms, at });
                    return self.keep_candidates();
                }
                false
            }
            PointerEvent::Shift(held) => {
                self.shift_held = held;
                false
            }
            PointerEvent::App { t_ms, name } => {
                if let Some(t_ms) = self.clock.recording_from_wall(t_ms) {
                    self.apps.push((t_ms, name));
                }
                false
            }
        }
    }

    /// A stroke from the drawing layer (epoch milliseconds).
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
        self.signals.strokes.push(InkStroke {
            t0_ms,
            t1_ms: t1_ms.max(t0_ms),
            points: points.into_iter().map(|p| (p.x, p.y)).collect(),
        });
        self.keep_candidates()
    }

    /// Runs the core candidate rules on the signals so far and asks the screen adapter to keep
    /// every picture that is new. Dwells cannot be judged live (they need the speech spans), so
    /// one span over the whole recording stands in; the plan at stop decides for real.
    fn keep_candidates(&mut self) -> bool {
        let Some(screen) = &self.screen else {
            return false;
        };
        let mut live = self.signals.clone();
        live.stopped_ms = self.clock.recording_now();
        live.speech = vec![SpeechSpan {
            start_ms: 0,
            end_ms: live.stopped_ms.saturating_add(1),
        }];
        let mut candidates = ann::candidates(&live);
        ann::prune_candidates(&mut candidates);
        let mut changed = false;
        for candidate in candidates {
            let Some(frame) = self.frame_of_sample.get(candidate.sample).copied() else {
                continue;
            };
            if self.kept.contains(&frame) {
                continue;
            }
            match screen.keep_frame(frame) {
                Ok(()) => {
                    self.kept.insert(frame);
                    changed = true;
                }
                Err(error) => tracing::debug!(%error, "candidate picture not kept"),
            }
        }
        changed
    }

    /// The app in front at each sample, from the names the pointer adapter reported.
    fn app_at(apps: &[(u64, String)], t_ms: u64) -> AppName {
        AppName(
            apps.iter()
                .rev()
                .find(|(t, _)| *t <= t_ms)
                .or_else(|| apps.first())
                .map(|(_, name)| name.clone())
                .unwrap_or_default(),
        )
    }
}

/// Everything the stop pipeline needs, moved out of the controller task.
pub struct PlanInput {
    pub signals: Signals,
    pub frame_of_sample: Vec<usize>,
    pub apps: Vec<(u64, String)>,
    pub kept: BTreeSet<usize>,
    pub candidates_dir: PathBuf,
    pub stopped_ms: u64,
}

/// Chooses the keyframes and makes sure each has a picture on disk. Pure work plus a few PNG
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
    for sample in &mut input.signals.samples {
        sample.app = AnnotationCtx::app_at(&input.apps, sample.t_ms);
    }
    let mut selection = ann::select_keyframes(&input.signals);

    // A picture for every keyframe: kept already, still in memory, or the nearest kept one.
    let mut kept: Vec<usize> = input.kept.iter().copied().collect();
    let mut pictures = Vec::with_capacity(selection.keyframes.len());
    let mut missing = Vec::new();
    for (i, keyframe) in selection.keyframes.iter_mut().enumerate() {
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
                        tracing::warn!(%error, "end picture not written");
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
    // A keyframe without any picture cannot be drawn: drop it and renumber.
    if !missing.is_empty() {
        tracing::warn!(
            dropped = missing.len(),
            "keyframes without a picture dropped"
        );
        let keep: Vec<bool> = (0..selection.keyframes.len())
            .map(|i| !missing.contains(&i))
            .collect();
        let mut n = 0;
        let mut renumber = vec![0usize; selection.keyframes.len() + 1];
        selection.keyframes = selection
            .keyframes
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep[*i])
            .map(|(i, mut k)| {
                n += 1;
                renumber[i + 1] = n;
                k.n = n;
                k
            })
            .collect();
        selection.marks.retain(|m| renumber[m.keyframe()] != 0);
        for mark in &mut selection.marks {
            let to = renumber[mark.keyframe()];
            match mark {
                ann::Mark::Ink { keyframe, .. }
                | ann::Mark::Click { keyframe, .. }
                | ann::Mark::Dwell { keyframe, .. } => *keyframe = to,
            }
        }
    }
    Planned {
        selection,
        pictures,
        regions: regions.to_vec(),
        total_samples,
    }
}

/// The audio ranges to transcribe, one keyframe each, and the chunk each becomes.
pub fn segments(planned: &Planned) -> Vec<pipeline::Segment> {
    let starts = ann::keyframe_starts_in_samples(&planned.selection.keyframes);
    pipeline::segment_by_keyframes(
        planned.total_samples,
        &planned.regions,
        &starts,
        ann::segment_lead_samples(),
    )
}

/// The chunk of a transcribed segment (keyframe index to the 1-based picture number).
pub fn chunk_of(segment: &pipeline::Segment, text: String) -> Chunk {
    Chunk {
        start_ms: ann::samples_to_ms(segment.speech.start),
        end_ms: ann::samples_to_ms(segment.speech.end),
        keyframe: segment.keyframe + 1,
        text: TranscriptText(text),
    }
}

/// What the compose step produced besides the pictures.
pub struct Composed {
    pub rendered: Rendered,
}

/// Draws the frames and the sheet and writes `summary.md` and `annotation.json`. Blocking.
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
) -> Result<Composed, String> {
    let mut pictures: Vec<Bitmap> = Vec::with_capacity(planned.pictures.len());
    for path in &planned.pictures {
        pictures.push(image::read_png(path).map_err(|e| e.to_string())?);
    }
    let frames: Vec<FrameInput<'_>> = planned
        .selection
        .keyframes
        .iter()
        .zip(&pictures)
        .map(|(k, picture)| FrameInput {
            n: k.n,
            picture,
            marks: &[],
        })
        .collect();
    // Marks per keyframe (borrowed slices need the per-keyframe vectors to live here).
    let per_keyframe: Vec<Vec<ann::Mark>> = planned
        .selection
        .keyframes
        .iter()
        .map(|k| {
            planned
                .selection
                .marks
                .iter()
                .filter(|m| m.keyframe() == k.n)
                .cloned()
                .collect()
        })
        .collect();
    let frames: Vec<FrameInput<'_>> = frames
        .into_iter()
        .zip(&per_keyframe)
        .map(|(f, marks)| FrameInput { marks, ..f })
        .collect();
    let rendered = sheet::render(&frames, bundle_dir).map_err(|e| e.to_string())?;
    let status = status_of(words, true);
    let summary = summary_text(
        language,
        duration_ms,
        Some(&rendered.sheet.to_string_lossy()),
        rendered.frames.len(),
        PasteTarget::Other,
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
        &planned.selection.keyframes,
        &planned.selection.marks,
        chunks,
        status,
    );
    bundles::write_bundle_files(bundle_dir, &summary, &json).map_err(|e| e.to_string())?;
    Ok(Composed { rendered })
}

/// The text to paste: with the sheet path (plain or `\ ` form) or words only.
#[allow(clippy::too_many_arguments)]
pub fn summary_text(
    language: Language,
    duration_ms: u64,
    sheet_path: Option<&str>,
    frame_count: usize,
    target: PasteTarget,
    planned: &Planned,
    chunks: &[Chunk],
    words: ComposeWords,
) -> String {
    ann::summary_text(&Summary {
        language,
        duration_ms,
        sheet_path,
        frame_count,
        target,
        keyframes: &planned.selection.keyframes,
        marks: &planned.selection.marks,
        chunks,
        transcription_failed: words == ComposeWords::Failed,
    })
}

/// The row status: failed words win, then a bundle without pictures, else ok.
pub fn status_of(words: ComposeWords, pictures: bool) -> AnnotationStatus {
    match (words, pictures) {
        (ComposeWords::Failed, _) => AnnotationStatus::Failed,
        (_, false) => AnnotationStatus::TextOnly,
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

/// Removes the candidate pictures after the save (the keyframes are in `frames/`).
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
    use murmur_core::annotations::{Keyframe, KeyframeReason, Mark};

    fn planned_with(keyframes: Vec<Keyframe>, marks: Vec<Mark>) -> Planned {
        Planned {
            selection: Selection { keyframes, marks },
            pictures: Vec::new(),
            regions: vec![SpeechRegion {
                start: 0,
                end: 16_000 * 5,
            }],
            total_samples: 16_000 * 10,
        }
    }

    fn keyframe(n: usize, t_ms: u64) -> Keyframe {
        Keyframe {
            n,
            t_ms,
            sample: n - 1,
            reasons: vec![KeyframeReason::Click],
            app: AppName::default(),
        }
    }

    #[test]
    fn chunks_use_one_based_picture_numbers() {
        let planned = planned_with(vec![keyframe(1, 0), keyframe(2, 3_000)], Vec::new());
        let segs = segments(&planned);
        assert_eq!(segs.len(), 1);
        let chunk = chunk_of(&segs[0], "hi".into());
        assert_eq!(chunk.keyframe, 1);
        assert_eq!(chunk.start_ms, 0);
        assert_eq!(chunk.end_ms, 5_000);
        // A segment of the second keyframe becomes chunk keyframe 2.
        let planned = Planned {
            regions: vec![SpeechRegion {
                start: 16_000 * 4,
                end: 16_000 * 6,
            }],
            ..planned
        };
        let segs = segments(&planned);
        assert_eq!(segs[0].keyframe, 1);
        assert_eq!(chunk_of(&segs[0], String::new()).keyframe, 2);
    }

    #[test]
    fn status_follows_words_then_pictures() {
        assert_eq!(
            status_of(ComposeWords::Transcribed, true),
            AnnotationStatus::Ok
        );
        assert_eq!(status_of(ComposeWords::None, true), AnnotationStatus::Ok);
        assert_eq!(
            status_of(ComposeWords::Transcribed, false),
            AnnotationStatus::TextOnly
        );
        assert_eq!(
            status_of(ComposeWords::Failed, true),
            AnnotationStatus::Failed
        );
        assert_eq!(
            status_of(ComposeWords::Failed, false),
            AnnotationStatus::Failed
        );
    }

    #[test]
    fn app_names_attach_to_samples_by_time() {
        let apps = vec![(0u64, "Safari".to_owned()), (5_000, "Notes".to_owned())];
        assert_eq!(AnnotationCtx::app_at(&apps, 100).0, "Safari");
        assert_eq!(AnnotationCtx::app_at(&apps, 5_000).0, "Notes");
        assert_eq!(AnnotationCtx::app_at(&apps, 9_000).0, "Notes");
        // Before the first report: the first known app; nothing known: empty.
        let late = vec![(2_000u64, "Mail".to_owned())];
        assert_eq!(AnnotationCtx::app_at(&late, 100).0, "Mail");
        assert_eq!(AnnotationCtx::app_at(&[], 100).0, "");
    }

    #[test]
    fn plan_drops_keyframes_without_any_picture_and_renumbers() {
        let dir = std::env::temp_dir().join(format!("murmur-plan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Three steady pictures; a click at 1.2 s makes the picture before it a candidate.
        let mut signals = Signals {
            stopped_ms: 4_000,
            display_pt: (1512.0, 982.0),
            ..Signals::default()
        };
        for i in 0..8u64 {
            signals.samples.push(ScreenSample {
                t_ms: i * 500,
                cells: vec![0.5; ann::GRID_CELLS],
                app: AppName::default(),
            });
        }
        signals.clicks.push(Click {
            t_ms: 1_200,
            at: (0.5, 0.5),
        });
        let frame_of_sample: Vec<usize> = (0..8).collect();
        // Only the click's picture (sample 2) was kept; the start picture (0) was not.
        let picture = Bitmap::new(4, 4).unwrap();
        image::write_png(&picture, &dir.join("2.png")).unwrap();
        let input = PlanInput {
            signals,
            frame_of_sample,
            apps: vec![(0, "Safari".into())],
            kept: [2usize].into_iter().collect(),
            candidates_dir: dir.clone(),
            stopped_ms: 4_000,
        };
        let planned = plan(
            input,
            &[SpeechRegion {
                start: 0,
                end: 16_000 * 4,
            }],
            16_000 * 4,
            &[],
        );
        // Start and click merge (within 1 s)? No: 0 and 1.2 s are 1.2 s apart, so two keyframes;
        // the start has no picture of its own and falls back to the nearest kept one.
        assert_eq!(planned.selection.keyframes.len(), 2);
        assert!(planned.pictures.iter().all(|p| p.ends_with("2.png")));
        let numbers: Vec<usize> = planned.selection.keyframes.iter().map(|k| k.n).collect();
        assert_eq!(numbers, vec![1, 2]);
        assert_eq!(planned.selection.keyframes[0].app.0, "Safari");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
