//! The model step of one dictation (experimental): makes the model calls that
//! `murmur_core::refine::Run` asks for, through any `TextRefiner` (the sidecar adapter in the
//! app, a fake in the tests), and warms the models at recording start.
//!
//! Runs on a blocking thread. Every call is bounded by the adapter (G5); the session's own timer
//! is in the core machine. No text reaches the log (G7).

use murmur_core::ports::{CancelToken, RefineError, TextRefiner};
use murmur_core::refine::{self, Glossary, Next, RefineKind, Run, RunReport};

/// Runs the calls of a refine session on the rule-formatted `text`. `on_loading` is called before
/// a call whose model is not loaded yet, so the machine can give the session the load budget.
pub fn drive(
    kind: RefineKind,
    text: &str,
    glossary: Glossary,
    refiner: &mut dyn TextRefiner,
    cancel: &CancelToken,
    on_loading: &mut dyn FnMut(),
) -> RunReport {
    let (mut run, mut call) = Run::start(kind, text, glossary);
    loop {
        let answer = if cancel.is_cancelled() {
            Err(RefineError::Cancelled)
        } else {
            if !refiner.ready(call.step) {
                on_loading();
            }
            refiner.generate(call.step, &call.request, cancel)
        };
        match run.advance(answer) {
            Next::Call(next) => call = next,
            Next::Done(report) => return report,
        }
    }
}

/// Starts and warms every model a session of `kind` may need (a translation may clean first,
/// E3). A failure only means the session's call starts or loads the model itself.
pub fn warm(kind: RefineKind, glossary: &Glossary, refiner: &mut dyn TextRefiner) {
    for step in refine::warm_steps(kind) {
        if let Err(error) = refiner.warm(*step, &refine::prefix(*step, glossary)) {
            tracing::info!(step = step.code(), %error, "refine warm-up did not finish");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use murmur_core::ports::{GenerateRequest, Generated};
    use murmur_core::refine::{ChatMessage, PreClean};
    use murmur_core::session::TranscriptText;

    use super::*;

    /// A refiner with scripted answers. A step is loaded by its first call.
    #[derive(Default)]
    struct Fake {
        answers: VecDeque<Result<Generated, RefineError>>,
        loaded: Vec<RefineKind>,
        calls: Vec<(RefineKind, GenerateRequest)>,
        warmed: Vec<(RefineKind, usize)>,
        /// Sets this token during the first call (the user cancels while it runs).
        cancel_during_call: Option<CancelToken>,
    }

    impl Fake {
        fn answering(answers: &[&str]) -> Fake {
            Fake {
                answers: answers
                    .iter()
                    .map(|a| {
                        Ok(Generated {
                            text: (*a).to_owned(),
                            truncated: false,
                        })
                    })
                    .collect(),
                ..Fake::default()
            }
        }
    }

    impl TextRefiner for Fake {
        fn warm(&mut self, step: RefineKind, prefix: &[ChatMessage]) -> Result<(), RefineError> {
            self.warmed.push((step, prefix.len()));
            self.loaded.push(step);
            Ok(())
        }

        fn ready(&self, step: RefineKind) -> bool {
            self.loaded.contains(&step)
        }

        fn generate(
            &mut self,
            step: RefineKind,
            request: &GenerateRequest,
            _cancel: &CancelToken,
        ) -> Result<Generated, RefineError> {
            self.calls.push((step, request.clone()));
            self.loaded.push(step);
            if let Some(token) = self.cancel_during_call.take() {
                token.cancel();
                return Err(RefineError::Cancelled);
            }
            self.answers
                .pop_front()
                .unwrap_or(Err(RefineError::Failed("no answer".to_owned())))
        }
    }

    const MARKED: &str = "会議は3時から、いや4時からでお願いします。";

    fn run(fake: &mut Fake, text: &str, cancel: &CancelToken) -> (RunReport, u32) {
        let mut loading = 0;
        let report = drive(
            RefineKind::Translate,
            text,
            Glossary::default(),
            fake,
            cancel,
            &mut || loading += 1,
        );
        (report, loading)
    }

    #[test]
    fn a_marked_translation_cleans_then_translates_and_reports_each_cold_model() {
        let mut fake = Fake::answering(&[
            "会議は4時からでお願いします。",
            "Please make the meeting at four.",
        ]);
        let (report, loading) = run(&mut fake, MARKED, &CancelToken::new());
        assert_eq!(
            report.result,
            Ok(TranscriptText(
                "Please make the meeting at four.".to_owned()
            ))
        );
        assert_eq!(report.pre_clean, PreClean::Used);
        assert_eq!(report.steps, vec![RefineKind::Clean, RefineKind::Translate]);
        let steps: Vec<_> = fake.calls.iter().map(|(s, _)| *s).collect();
        assert_eq!(steps, report.steps);
        // Neither model was loaded: both calls were announced as loads.
        assert_eq!(loading, 2);
    }

    #[test]
    fn warm_models_are_not_reported_as_loading() {
        let mut fake = Fake::answering(&[
            "会議は4時からでお願いします。",
            "Please make the meeting at four.",
        ]);
        warm(RefineKind::Translate, &Glossary::default(), &mut fake);
        // The translation model first, then the cleanup model for the marked texts.
        assert_eq!(
            fake.warmed.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![RefineKind::Translate, RefineKind::Clean]
        );
        let (report, loading) = run(&mut fake, MARKED, &CancelToken::new());
        assert!(report.result.is_ok());
        assert_eq!(loading, 0);
    }

    #[test]
    fn a_failed_cleanup_translates_the_rule_text() {
        let mut fake = Fake::default();
        fake.answers.push_back(Err(RefineError::Timeout));
        fake.answers.push_back(Ok(Generated {
            text: "Please make the meeting at three, no, four.".to_owned(),
            truncated: false,
        }));
        let (report, _) = run(&mut fake, MARKED, &CancelToken::new());
        assert_eq!(report.pre_clean, PreClean::Failed(RefineError::Timeout));
        assert!(report.result.is_ok());
        assert_eq!(
            fake.calls[1].1.messages.last().map(|m| m.content.as_str()),
            Some(MARKED)
        );
    }

    #[test]
    fn a_cancel_stops_before_the_next_call() {
        // Cancelled before anything ran: no call at all.
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut fake = Fake::answering(&["x"]);
        let (report, loading) = run(&mut fake, MARKED, &cancel);
        assert_eq!(report.result, Err(RefineError::Cancelled));
        assert!(fake.calls.is_empty());
        assert_eq!(loading, 0);
        // Cancelled during the cleanup: the translation does not run.
        let cancel = CancelToken::new();
        let mut fake = Fake {
            cancel_during_call: Some(cancel.clone()),
            ..Fake::answering(&["Please make the meeting at four."])
        };
        let (report, _) = run(&mut fake, MARKED, &cancel);
        assert_eq!(report.result, Err(RefineError::Cancelled));
        assert_eq!(fake.calls.len(), 1);
    }

    #[test]
    fn an_unmarked_translation_makes_one_call() {
        let mut fake = Fake::answering(&["The meeting is at four."]);
        let (report, _) = run(&mut fake, "会議は4時からです。", &CancelToken::new());
        assert!(report.result.is_ok());
        assert_eq!(report.steps, vec![RefineKind::Translate]);
        assert_eq!(fake.calls.len(), 1);
    }
}
