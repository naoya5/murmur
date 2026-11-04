//! Development-only prompt transport for the sidecar benchmark, not an app entry point.
//! The caller captures stdout in memory; never print or persist it as a log.

use murmur_core::refine::{self, Glossary, RefineKind};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let lists = match std::env::args().nth(1).as_deref() {
        None | Some("lists-on") => true,
        Some("lists-off") => false,
        Some(_) => return Err("expected lists-on or lists-off".into()),
    };
    let messages: Vec<_> = refine::prefix(RefineKind::Clean, &Glossary::default(), lists)
        .into_iter()
        .map(|message| json!({"role": message.role.as_str(), "content": message.content}))
        .collect();
    serde_json::to_writer(std::io::stdout().lock(), &messages)?;
    Ok(())
}
