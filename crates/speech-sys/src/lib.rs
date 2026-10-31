//! Raw FFI bindings to the C API of speech.cpp (`include/speech.h`).
//!
//! The library is built from source by `build.rs` with CMake and linked statically, together with
//! ggml and, on macOS, the Metal backend with its shader library embedded. Nothing here is safe to
//! call without reading the header's contract; safe wrappers belong to the engine adapter, not to
//! this crate.
//!
//! Rules of the C API that every caller must respect (see `docs/c-api.md` of speech.cpp):
//!
//! - Every object is freed only through the function named for it, requests and detections before
//!   their model, and every model before the process exits. On Metal, a process that exits with a
//!   model still loaded aborts.
//! - A model serves one request at a time; a request is used by one thread at a time, except
//!   `speech_request_cancel`, which any thread may call while the request exists.
//! - `speech_last_error` and `speech_last_error_option` are per thread and valid until the next
//!   call into the library on that thread. Read them right after the failing call.
//! - Enums are generated as plain integer constants (`SPEECH_OK`, `SPEECH_OPT_LANGUAGE`, ...) with
//!   type aliases (`speech_status`, `speech_option`, ...), so a value the header does not know is
//!   never undefined behaviour.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    clippy::missing_safety_doc,
    clippy::too_many_arguments,
    clippy::upper_case_acronyms,
    rustdoc::broken_intra_doc_links,
    rustdoc::bare_urls,
    rustdoc::invalid_html_tags
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
