//! Shared core, target-agnostic on purpose. See ../../HANDOVER.md and
//! docs/architecture.md before writing real logic here.

pub mod datetime;
pub mod field_label;
pub mod field_log;
pub mod filename;
pub mod mix;
pub mod schedule;
pub mod storage;
pub mod text;

pub use datetime::DateTime;
pub use field_label::{parse_field_label, FieldKind, ParsedField};
pub use field_log::{build_log_body, parse_log_body, FieldSection};
pub use filename::{safe_filename, InvalidFilename};
pub use mix::{format_mix, mix_total, order_mix, parse_mix, round_weight, MixPart};
pub use schedule::{compute_next_due, format_cadence, format_due, Cadence, DueInfo, Schedule, Status};
#[cfg(not(target_arch = "wasm32"))]
pub use storage::FsStore;
pub use storage::{Store, StoreError};
pub use text::title_case;
