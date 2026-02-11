//! Platform-specific constants and helpers.

/// `OpenOptionsExt::custom_flags` value for enabling `O_NOFOLLOW` where supported.
///
/// This prevents following a symlink on `open(2)`, helping mitigate TOCTOU attacks.
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
pub(crate) const O_NOFOLLOW_FLAG: i32 = 0o400000;

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )
))]
pub(crate) const O_NOFOLLOW_FLAG: i32 = 0x0100;

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
pub(crate) const O_NOFOLLOW_FLAG: i32 = 0;
