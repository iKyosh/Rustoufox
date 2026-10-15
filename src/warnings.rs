//! The warnings the launcher emits: leak warnings for options that can cause
//! detection, and fallback warnings when part of an identity could not be
//! drawn. They are printed to stderr, and every warning raised while building
//! a launch is also collected on the resulting options.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::paths::data;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarningKind {
    Leak,
    Fallback,
    Runtime,
    Future,
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub kind: WarningKind,
    pub message: String,
}

thread_local! {
    static COLLECTED: RefCell<Option<Vec<Warning>>> = const { RefCell::new(None) };
}

fn warnings_data() -> &'static HashMap<String, String> {
    static D: OnceLock<HashMap<String, String>> = OnceLock::new();
    D.get_or_init(|| serde_yaml::from_str(data::WARNINGS_YML).expect("warnings.yml"))
}

/// Start collecting warnings on this thread (in addition to printing them).
pub(crate) fn start_collecting() {
    COLLECTED.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

/// Stop collecting and return what was collected.
pub(crate) fn stop_collecting() -> Vec<Warning> {
    COLLECTED.with(|c| c.borrow_mut().take().unwrap_or_default())
}

pub fn emit(kind: WarningKind, message: impl Into<String>) {
    let message = message.into();
    let label = match kind {
        WarningKind::Leak => "LeakWarning",
        WarningKind::Fallback => "FallbackWarning",
        WarningKind::Runtime => "RuntimeWarning",
        WarningKind::Future => "FutureWarning",
    };
    // One launch says the same thing once.
    let repeated = COLLECTED
        .with(|c| c.borrow().as_ref().is_some_and(|list| list.iter().any(|w| w.kind == kind && w.message == message)));
    if repeated {
        return;
    }
    if std::env::var_os("RUSTOUFOX_QUIET_WARNINGS").is_none() {
        eprintln!("{label}: {message}");
    }
    COLLECTED.with(|c| {
        if let Some(list) = c.borrow_mut().as_mut() {
            list.push(Warning { kind, message });
        }
    });
}

/// The text of a warning in `warnings.yml`.
pub fn text(key: &str) -> String {
    warnings_data().get(key).cloned().unwrap_or_else(|| key.to_string())
}

/// Warn that a passed parameter can cause leaks, unless the caller said they
/// know what they are doing. `i_know_what_im_doing = None` means the warning
/// cannot be silenced.
pub fn leak(key: &str, i_know_what_im_doing: Option<bool>) {
    if i_know_what_im_doing == Some(true) {
        return;
    }
    let mut message = text(key);
    if i_know_what_im_doing.is_some() {
        message.push_str("\nIf this is intentional, pass `i_know_what_im_doing=True`.");
    }
    emit(WarningKind::Leak, message);
}

/// Warn that `what` failed with `error` and the identity uses `instead`.
pub fn fallback(what: &str, instead: &str, error: &str, identity: Option<&str>) {
    let mut lines = vec![
        format!("rustoufox: {}", env!("CARGO_PKG_VERSION")),
        format!("browser: {}", crate::pkgman::installed_verstr().unwrap_or_else(|_| "not installed".into())),
        format!("os: {} {}", std::env::consts::OS, std::env::consts::ARCH),
        format!("error: {error}"),
    ];
    if let Some(identity) = identity {
        lines.push(format!("identity: {identity}"));
    }
    let report = lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
    let message = text("fallback").replace("{what}", what).replace("{instead}", instead).replace("{report}", &report);
    emit(WarningKind::Fallback, message);
}

pub fn runtime(message: impl Into<String>) {
    emit(WarningKind::Runtime, message);
}
