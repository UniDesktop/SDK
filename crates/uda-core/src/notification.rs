use crate::capability::Capability;
use crate::error::UdaError;

/// Urgency level for a notification, following the FreeDesktop Notifications spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    /// Low urgency. No special display.
    Low = 0,
    /// Normal urgency. Default behavior.
    #[default]
    Normal = 1,
    /// Critical urgency. Displayed immediately, may override screen lock.
    Critical = 2,
}

/// Represents a desktop notification to be sent via the system notification service.
///
/// Maps directly to the FreeDesktop `org.freedesktop.Notifications.Notify` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The application name (sender).
    pub app_name: String,
    /// Optional ID of a notification to replace. 0 means a new notification.
    pub replaces_id: u32,
    /// Path or URI to an icon image.
    pub app_icon: String,
    /// One-line summary / title of the notification.
    pub summary: String,
    /// Multi-line body of the notification.
    pub body: String,
    /// Action pairs (key, localized_label). Flattened for transport internally.
    pub actions: Vec<(String, String)>,
    /// Timeout in milliseconds. 0 = server default, -1 = never expire.
    pub expire_timeout: i32,
    /// Urgency level, transported as the `urgency` hint.
    pub urgency: Urgency,
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            app_name: "UDA".to_string(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            expire_timeout: 0,
            urgency: Urgency::default(),
        }
    }
}

/// Decide what a notification card's image should display, if anything.
///
/// Returns `None` for a value that carries no image, so the caller can degrade
/// to a text-only card. Otherwise returns the value normalised into the form
/// the platform resolves. Both backends need this answer, which is why it lives
/// in the core rather than in either platform crate.
///
/// * A bare filesystem path becomes a `file://` URI. This is not cosmetic: the
///   Windows toast platform resolves `src` from the *shell's* context rather
///   than the sender's working directory, so `C:\pics\a.png` is not reliably
///   located even though the file is right there.
/// * A value that already carries a scheme (`file:`, `http:`, `https:`,
///   `ms-appx:`, ...) is passed through untouched, so a caller that built a URI
///   already is not double-prefixed into `file:///file:///...`.
/// * The FreeDesktop backend on Linux accepts a plain path and needs no
///   conversion, but the same normalisation is harmless there: a path with no
///   scheme and no leading separator is returned unchanged.
///
/// Only the *syntax* is decided. The file's existence is deliberately not
/// checked: a typo would then become a hard failure, whereas the platforms'
/// own answer for an unreadable image is to draw the card without one - the
/// same degradation an empty value already gets.
pub fn image_source(app_icon: &str) -> Option<String> {
    let trimmed = app_icon.trim();
    if trimmed.is_empty() {
        return None;
    }

    if has_uri_scheme(trimmed) {
        return Some(trimmed.to_string());
    }

    Some(file_uri(trimmed))
}

/// Whether `value` starts with a URI scheme rather than a Windows drive letter.
///
/// RFC 3986 requires a scheme to begin with a letter. It also allows a
/// single-character scheme, so the length test cannot do the work of telling a
/// drive letter apart - but a drive letter only appears before a colon in the
/// two-character form `C:`, where the prefix is exactly one character long. The
/// "at least two" rule below is what keeps `C:/pics` on the conversion path.
pub fn has_uri_scheme(value: &str) -> bool {
    let Some((prefix, _)) = value.split_once(':') else {
        return false;
    };

    if prefix.len() < 2 {
        return false;
    }

    let mut characters = prefix.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }

    prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Build a `file://` URI from a filesystem path.
///
/// Backslashes become forward slashes because the URI grammar treats `\` as an
/// ordinary character, so a Windows path kept verbatim would not resolve.
/// Absolute paths get the authority-less form `file:///C:/...`; relative ones
/// keep what the caller supplied, because UDA has no working directory to
/// resolve them against and inventing one would be worse than passing the value
/// through.
///
/// Absolute-ness comes from [`is_absolute_path`], which is host-independent.
pub fn file_uri(path: &str) -> String {
    let forward = path.replace('\\', "/");

    if !is_absolute_path(&forward) {
        return forward;
    }

    // Exactly one slash must follow the `file://` authority marker. Concatenating
    // blindly gives `file:////home/...` when the path already starts with a
    // separator, and `file://C:/...` when it does not.
    format!("file:///{}", forward.trim_start_matches('/'))
}

/// Whether `path` names a location that does not depend on a working directory,
/// on either platform's syntax.
///
/// Two forms count: a leading separator (`/home/u/a.png`) and a drive-letter
/// root (`C:/a.png`). `std::path::Path::is_absolute()` is deliberately *not*
/// used, because it is host-dependent: on the Linux host that runs CI it reports
/// `C:/pics/a.png` as relative, since POSIX accepts only a leading separator as a
/// root. A Windows caller's icon would then silently lose its `file:///` prefix,
/// and the tests asserting that form would pass nowhere.
///
/// Both separators are accepted after the drive letter, so the answer holds
/// whether or not the caller has already normalised the string.
fn is_absolute_path(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }

    // `C:/...` - one ASCII letter, a colon, and a separator, i.e. the root of a
    // drive rather than a stream name (`C:foo`) or a bare drive (`C:`).
    let mut characters = path.chars();
    let Some(drive) = characters.next() else {
        return false;
    };
    if !drive.is_ascii_alphabetic() {
        return false;
    }
    if characters.next() != Some(':') {
        return false;
    }

    matches!(characters.next(), Some('/' | '\\'))
}

/// Decide what a card's `alt` text (the description a screen reader reads for
/// the image) should be, if the caller named itself.
///
/// A blank name yields `None` so the caller can substitute a generic word: an
/// empty `alt` is schema-valid on some platforms and silent to a screen reader.
pub fn header_title(app_name: &str) -> Option<&str> {
    let trimmed = app_name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Cross-platform notification management interface.
#[async_trait::async_trait]
pub trait NotificationManager {
    /// Send a notification and return the notification ID assigned by the server.
    async fn send(&self, notification: &Notification) -> Result<u32, UdaError>;

    /// Return the set of capabilities supported by the current backend.
    fn capabilities(&self) -> Result<Capability, UdaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_default_is_normal() {
        assert_eq!(Urgency::default(), Urgency::Normal);
    }

    #[test]
    fn urgency_values_match_spec() {
        assert_eq!(Urgency::Low as u8, 0);
        assert_eq!(Urgency::Normal as u8, 1);
        assert_eq!(Urgency::Critical as u8, 2);
    }

    #[test]
    fn notification_default_has_normal_urgency() {
        let n = Notification::default();
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn notification_default_has_zero_replaces_id() {
        let n = Notification::default();
        assert_eq!(n.replaces_id, 0);
    }

    // ------------------------------------------------------------------
    // 图标来源规范化
    //
    // 这些是平台无关的纯函数，刻意放在 core：Windows 后端所在 crate 是
    // `#![cfg(windows)]`，写在那边的测试在 Linux CI 上编译为空，等于没人验证。
    // ------------------------------------------------------------------

    #[test]
    fn a_windows_path_becomes_a_file_uri() {
        // toast 平台从 shell 自己的上下文解析 `src`，裸路径定位不可靠。
        assert_eq!(
            image_source(r"C:\pics\icon.png").as_deref(),
            Some("file:///C:/pics/icon.png")
        );
    }

    #[test]
    fn a_posix_path_becomes_a_file_uri() {
        assert_eq!(
            image_source("/home/user/icon.png").as_deref(),
            Some("file:///home/user/icon.png")
        );
    }

    #[test]
    fn an_existing_scheme_is_left_alone() {
        // 已构造 URI 的调用方不该被二次加前缀，否则会得到
        // `file:///file:///...` 这种必然解析失败的形态。
        for value in [
            "file:///C:/pics/icon.png",
            "http://example.com/icon.png",
            "https://example.com/icon.png",
            "ms-appx:///Assets/icon.png",
        ] {
            assert_eq!(image_source(value).as_deref(), Some(value));
        }
    }

    #[test]
    fn a_drive_letter_is_not_mistaken_for_a_scheme() {
        // `C:` 的冒号前只有一个字符，因此不满足"至少两个"规则，路径走转换分支。
        assert!(!has_uri_scheme("C:/pics/icon.png"));
        assert!(!has_uri_scheme("c:\\pics"));
        assert!(has_uri_scheme("file:/a.png"));
        assert!(has_uri_scheme("https://example.com"));
    }

    #[test]
    fn a_value_with_no_colon_is_a_plain_path() {
        assert!(!has_uri_scheme("pics/icon.png"));
        assert!(!has_uri_scheme("icon.png"));
    }

    #[test]
    fn absolute_paths_are_recognised_regardless_of_the_host() {
        // Pins the host-independent contract on the machine CI actually uses,
        // where `Path::is_absolute()` would answer the opposite (see
        // `is_absolute_path`).
        assert!(is_absolute_path("/home/user/icon.png"));
        assert!(is_absolute_path("C:/pics/icon.png"));
        assert!(is_absolute_path("C:\\pics\\icon.png"));

        assert!(!is_absolute_path("pics/icon.png"));
        assert!(!is_absolute_path("icon.png"));
        assert!(!is_absolute_path(""));
        // A bare drive and a stream name are not roots.
        assert!(!is_absolute_path("C:"));
        assert!(!is_absolute_path("C:icon.png"));
        // One letter before the colon is the minimum, so `:x` is not a drive.
        assert!(!is_absolute_path(":/icon.png"));
    }

    #[test]
    fn a_scheme_needs_a_letter_first() {
        // `1http:` 与 `-file:` 不是 scheme，应按路径处理。
        assert!(!has_uri_scheme("1http://x"));
        assert!(!has_uri_scheme("-file://x"));
    }

    #[test]
    fn an_empty_or_blank_icon_yields_no_source() {
        // 空值不得产出 `<image src=""/>`——schema 不接受。
        assert!(image_source("").is_none());
        assert!(image_source("   ").is_none());
        assert!(image_source("\t\n").is_none());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_off_the_source() {
        assert_eq!(
            image_source("  /home/user/icon.png  ").as_deref(),
            Some("file:///home/user/icon.png")
        );
    }

    #[test]
    fn backslashes_become_forward_slashes() {
        // URI 语法把 `\` 当普通字符，保留分隔符将无法解析。
        let source = image_source(r"D:\a\b\c.png").expect("a path yields a source");
        assert!(!source.contains('\\'), "backslash survived: {source}");
        assert_eq!(source, "file:///D:/a/b/c.png");
    }

    #[test]
    fn a_relative_path_is_not_given_an_authority() {
        // UDA 没有可用于解析的工作目录，编造一个比原样传递更糟。
        assert_eq!(image_source("pics/icon.png").as_deref(), Some("pics/icon.png"));
    }

    #[test]
    fn the_linux_backend_accepts_the_same_plain_path() {
        // Linux 后端直接吃裸路径。规范化对无 scheme、无前导分隔符的值是恒等的，
        // 因此两个平台可以用同一个入口而不必分支。
        assert_eq!(image_source("pics/icon.png").as_deref(), Some("pics/icon.png"));
        assert_eq!(file_uri("pics/icon.png"), "pics/icon.png");
    }

    // ------------------------------------------------------------------
    // alt 文本
    // ------------------------------------------------------------------

    #[test]
    fn alt_text_comes_from_the_app_name_and_falls_back() {
        assert_eq!(header_title("UDA Notification Demo"), Some("UDA Notification Demo"));
        assert_eq!(header_title("  UDA  "), Some("UDA"));
        assert_eq!(header_title(""), None);
    }
}
