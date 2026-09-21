//! Windows platform backend for UniDesktop API (UDA).
//!
//! Every public item in this crate is gated behind [`cfg(windows)`], so the
//! workspace still type-checks on Linux/macOS hosts (see `AGENTS.md`,
//! Principle 3: Cross-Compilation Hygiene). The gate is declared **before** the
//! module list so that a stray module declaration can never be compiled on a
//! non-Windows target.
//!
//! # Modules
//!
//! - [`appearance`]: dark/light detection via the `AppsUseLightTheme` registry value.
//! - [`wallpaper`]: static wallpaper via `SystemParametersInfoW` + registry style keys.
//! - [`wakelock`]: display/system sleep inhibition via `SetThreadExecutionState`.
//! - [`detection`]: OS release detection via `RtlGetVersion`.
//! - [`notification`]: native toasts via the WinRT `ToastNotificationManager`.
//!
//! # Fallback tiers
//!
//! Unlike Linux (see `AGENTS.md` Principle 2), Windows has a single tier per
//! feature: the Win32/WinRT API is always available on a supported build, so
//! there is no portal, no desktop-shell IPC, and no CLI fallback chain. The
//! `Tier 4` graceful error still applies: when a feature genuinely cannot be
//! delivered (for example dark-mode detection on Windows 7), the code returns
//! [`UdaError::NotSupported`](uda_core::error::UdaError::NotSupported) rather
//! than panicking.
//!
//! # Error handling
//!
//! Following `AGENTS.md` Principle 1 (Never Panic), no Win32 call in this crate
//! uses `unwrap()`/`expect()`. Every FFI result is inspected and mapped into a
//! strongly typed [`UdaError`](uda_core::error::UdaError).

#![cfg(windows)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod appearance;
pub mod detection;
pub mod notification;
pub mod wakelock;
pub mod wallpaper;
