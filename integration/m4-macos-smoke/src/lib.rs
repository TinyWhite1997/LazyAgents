//! M4 macOS smoke crate (WEK-74 / M4.4 A6).
//!
//! Empty library — every assertion lives in `tests/` and is cfg-gated
//! on `cfg(target_os = "macos")` so building on Linux/Windows yields a
//! no-op crate.
