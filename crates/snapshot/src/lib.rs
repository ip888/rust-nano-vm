//! Snapshot + fork primitive — the M5 wedge.
//!
//! Scope: **M5**. This is where the < 50 ms cold-start target is earned.
//! Placeholder so the workspace compiles. Real implementation lands with M5
//! using `userfaultfd` and copy-on-write memory sharing across children.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
