macro_rules! assert_eq_pretty {
    ($left:expr, $right:expr $(,)?) => {
        {
            let expected = $left;
            let actual = $right;
            std::assert!(expected.eq(&actual), "expected: {:#?}, actual: {:#?}", expected, actual);
        }
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {
        {
            let expected = $left;
            let actual = $right;
            std::assert!(expected.eq(&actual), "{}, expected: {:#?}, actual: {:#?}", std::format_args!($($arg)+), expected, actual);
        }
    }
}

/// There is assert_matches in std, but it is unstable. As always.
///
/// assert_matches!(value, Pattern::Foo);
macro_rules! assert_matches {
    ($left:expr, $pattern:pat $(,)?) => {{
        match $left {
            $pattern => {}
            ref unexpected => {
                panic!(
                    "assertion failed: value `{:#?}` doesn't match pattern `{}`\n",
                    unexpected,
                    stringify!($pattern),
                )
            }
        }
    }};
    ($left:expr, $pattern:pat, $($arg:tt)+) => {{
        match $left {
            $pattern => {}
            ref unexpected => {
                panic!(
                    "{}; value `{:#?}` doesn't match pattern `{}`\n",
                    std::format_args!($($arg)+),
                    unexpected,
                    stringify!($pattern),
                )
            }
        }
    }};
}

pub(crate) use {assert_eq_pretty, assert_matches};
