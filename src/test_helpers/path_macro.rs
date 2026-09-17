/// Macro to create platform-dependent root path.
/// On Windows: `C:\`
/// On Unix: `/`
#[macro_export]
macro_rules! root {
    () => {{
        #[cfg(windows)]
        {
            std::path::PathBuf::from("C:\\")
        }
        #[cfg(not(windows))]
        {
            std::path::PathBuf::from("/")
        }
    }};
}

/// Macro to create platform-dependent paths from segments.
/// On Windows: `C:\segment1\segment2\...`
/// On Unix: `/segment1/segment2/...`
///
/// The name clashes with the built-in `path` attribute, so a plain `pub(crate) use` is ambiguous.
/// `macro_export` puts it at the crate root instead, and the parent module re-exports it from there.
#[macro_export]
macro_rules! path {
    ($($segment:expr),+ $(,)?) => {{
        #[cfg(windows)]
        {
            let mut path = std::path::PathBuf::from("C:\\");
            $(
                path.push($segment);
            )+
            path
        }
        #[cfg(not(windows))]
        {
            let mut path = std::path::PathBuf::from("/");
            $(
                path.push($segment);
            )+
            path
        }
    }};
}
