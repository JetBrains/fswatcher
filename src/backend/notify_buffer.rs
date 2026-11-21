//! A bounded reader of the buffer `ReadDirectoryChangesExW` fills.
//!
//! The kernel writes a chain of `FILE_NOTIFY_INFORMATION` or `FILE_NOTIFY_EXTENDED_INFORMATION` entries.
//! Each entry names the offset of the next one and the byte length of its file name.
//! The reader takes the bytes one read transferred and never reads outside them.
//! An entry that does not fit them is an error.
//!
//! The module is byte arithmetic over the Win32 layouts, so its tests run on every platform.
//! On Windows, `windows/read_directory_changes.rs` checks the layouts against the `windows` crate's types at compile time.
//! The meaning of an action or an attribute is left to the Windows backend.

use std::fmt;

/// The field offsets of one entry layout, in bytes from the entry's start. `NextEntryOffset: u32` is at 0 in both layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntryLayout {
    /// `Action: u32`.
    pub action: usize,
    /// `FileAttributes: u32`. The basic layout has none.
    pub attributes: Option<usize>,
    /// `FileNameLength: u32`, a count of bytes.
    pub name_length: usize,
    /// `FileName`: UTF-16 code units, little-endian, not terminated. The fixed header ends here.
    pub name: usize,
}

/// `FILE_NOTIFY_INFORMATION`.
pub(crate) const BASIC: EntryLayout = EntryLayout {
    action: 4,
    attributes: None,
    name_length: 8,
    name: 12,
};

/// `FILE_NOTIFY_EXTENDED_INFORMATION`.
pub(crate) const EXTENDED: EntryLayout = EntryLayout {
    action: 4,
    attributes: Some(56),
    name_length: 80,
    name: 84,
};

/// The actions a directory read reports: `FILE_ACTION_ADDED` (1) to `FILE_ACTION_RENAMED_NEW_NAME` (5).
pub(crate) const KNOWN_ACTIONS: std::ops::RangeInclusive<u32> = 1..=5;

/// One entry of the chain, copied out of the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// One of [`KNOWN_ACTIONS`].
    pub action: u32,
    /// `FileAttributes` of the extended layout.
    pub attributes: Option<u32>,
    /// The file name relative to the watched directory, as UTF-16 code units.
    pub name: Vec<u16>,
}

/// The chain does not fit the bytes the read transferred, or an entry is not one the kernel writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Malformed {
    /// The offset of the entry that failed.
    pub offset: usize,
    pub reason: &'static str,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the entry at byte {}: {}", self.offset, self.reason)
    }
}

const HEADER: &str = "its header does not fit the bytes read";

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

/// Reads the entries of one completed read. `bytes` is exactly what the read transferred.
///
/// Every access is checked against `bytes`. The chain must move forward: the next entry starts behind this one's name.
/// So the loop ends, and it reads nothing outside `bytes`.
pub(crate) fn read_entries(bytes: &[u8], layout: EntryLayout) -> Result<Vec<RawEntry>, Malformed> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    loop {
        let malformed = move |reason| Malformed { offset: at, reason };
        let field = move |offset: usize| {
            at.checked_add(offset)
                .and_then(|field| u32_at(bytes, field))
                .ok_or(malformed(HEADER))
        };
        let next = field(0)? as usize;
        let action = field(layout.action)?;
        let attributes = match layout.attributes {
            Some(offset) => Some(field(offset)?),
            None => None,
        };
        let name_length = field(layout.name_length)? as usize;
        if !KNOWN_ACTIONS.contains(&action) {
            return Err(malformed("its action is not one a directory read reports"));
        }
        if !name_length.is_multiple_of(2) {
            return Err(malformed("its name length is odd"));
        }
        // `at + layout.name` fits: the header fields behind it were read above.
        let name_start = at + layout.name;
        let name = name_start
            .checked_add(name_length)
            .and_then(|name_end| bytes.get(name_start..name_end))
            .ok_or(malformed("its name runs past the bytes read"))?;
        entries.push(RawEntry {
            action,
            attributes,
            // The length is even (checked above), so the remainder of `as_chunks` is empty.
            name: name.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect(),
        });
        if next == 0 {
            return Ok(entries);
        }
        if next < layout.name + name_length {
            return Err(malformed("the next entry starts inside it"));
        }
        at = at.checked_add(next).ok_or(malformed("the next entry's offset overflows"))?;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const ADDED: u32 = 1;
    const MODIFIED: u32 = 3;
    const DIRECTORY: u32 = 0x10;

    fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn units(name: &str) -> Vec<u16> {
        name.encode_utf16().collect()
    }

    /// One entry as the kernel writes it: the header, the name, then padding to the layout's alignment.
    /// `last` writes a zero `NextEntryOffset`; otherwise the offset is the entry's padded length.
    fn entry(layout: EntryLayout, action: u32, attributes: u32, name: &str, last: bool) -> Vec<u8> {
        let name = units(name);
        let align = if layout.attributes.is_some() { 8 } else { 4 };
        let length = (layout.name + name.len() * 2).next_multiple_of(align);
        let mut bytes = vec![0u8; length];
        put_u32(&mut bytes, 0, if last { 0 } else { u32::try_from(length).unwrap() });
        put_u32(&mut bytes, layout.action, action);
        if let Some(offset) = layout.attributes {
            put_u32(&mut bytes, offset, attributes);
        }
        put_u32(&mut bytes, layout.name_length, u32::try_from(name.len() * 2).unwrap());
        for (index, unit) in name.iter().enumerate() {
            let at = layout.name + index * 2;
            bytes[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn reason(result: Result<Vec<RawEntry>, Malformed>) -> (usize, &'static str) {
        let malformed = result.expect_err("the buffer is malformed");
        (malformed.offset, malformed.reason)
    }

    #[test]
    fn an_extended_chain_is_read_entry_by_entry() {
        let mut bytes = entry(EXTENDED, ADDED, DIRECTORY, "a\\dir", false);
        bytes.extend(entry(EXTENDED, MODIFIED, 0, "a\\dir\\file.txt", true));
        let entries = read_entries(&bytes, EXTENDED).unwrap();
        assert_eq!(
            entries,
            vec![
                RawEntry {
                    action: ADDED,
                    attributes: Some(DIRECTORY),
                    name: units("a\\dir"),
                },
                RawEntry {
                    action: MODIFIED,
                    attributes: Some(0),
                    name: units("a\\dir\\file.txt"),
                },
            ]
        );
    }

    #[test]
    fn a_basic_chain_carries_no_attributes() {
        let mut bytes = entry(BASIC, ADDED, 0, "x", false);
        bytes.extend(entry(BASIC, MODIFIED, 0, "y.txt", true));
        let entries = read_entries(&bytes, BASIC).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].attributes, None);
        assert_eq!(entries[1].name, units("y.txt"));
    }

    #[test]
    fn an_empty_name_is_the_watched_directory_itself() {
        let bytes = entry(EXTENDED, MODIFIED, DIRECTORY, "", true);
        assert_eq!(read_entries(&bytes, EXTENDED).unwrap()[0].name, Vec::<u16>::new());
    }

    /// A buffer no read has filled can hold any name length. An unbounded reader builds a slice of that length
    /// and reads far past the buffer: an access violation.
    #[test]
    fn a_name_length_of_garbage_is_refused_not_read() {
        let mut bytes = entry(EXTENDED, ADDED, 0, "name", true);
        put_u32(&mut bytes, EXTENDED.name_length, 0x10ED_3998);
        assert_eq!(reason(read_entries(&bytes, EXTENDED)), (0, "its name runs past the bytes read"));
        put_u32(&mut bytes, EXTENDED.name_length, u32::MAX - 1);
        assert_eq!(reason(read_entries(&bytes, EXTENDED)), (0, "its name runs past the bytes read"));
    }

    /// A zeroed buffer read as an entry has action 0, which is no file action.
    #[test]
    fn a_zeroed_buffer_is_refused() {
        let bytes = vec![0u8; 512 * 1024];
        assert_eq!(
            reason(read_entries(&bytes, EXTENDED)),
            (0, "its action is not one a directory read reports")
        );
        assert_eq!(
            reason(read_entries(&bytes, BASIC)),
            (0, "its action is not one a directory read reports")
        );
    }

    #[test]
    fn an_unknown_action_is_refused() {
        let bytes = entry(EXTENDED, 6, 0, "stream", true);
        assert_eq!(
            reason(read_entries(&bytes, EXTENDED)),
            (0, "its action is not one a directory read reports")
        );
    }

    #[test]
    fn a_read_shorter_than_one_header_is_refused() {
        let bytes = entry(EXTENDED, ADDED, 0, "name", true);
        for length in [0, 3, 4, EXTENDED.name - 1] {
            assert_eq!(
                reason(read_entries(&bytes[..length], EXTENDED)),
                (0, "its header does not fit the bytes read"),
                "{length} bytes"
            );
        }
    }

    /// The bytes behind what the read transferred are not the kernel's: the reader stops at the transferred length.
    #[test]
    fn a_chain_that_leaves_the_bytes_read_is_refused() {
        let first = entry(EXTENDED, ADDED, 0, "first", false);
        let mut bytes = first.clone();
        bytes.extend(entry(EXTENDED, ADDED, 0, "second", true));
        // The read transferred the first entry only; its offset points behind it.
        assert_eq!(
            reason(read_entries(&bytes[..first.len()], EXTENDED)),
            (first.len(), "its header does not fit the bytes read")
        );
        // The second entry's name is cut.
        assert_eq!(
            reason(read_entries(&bytes[..bytes.len() - 8], EXTENDED)),
            (first.len(), "its name runs past the bytes read")
        );
        // An offset far outside.
        put_u32(&mut bytes, 0, 0x7FFF_FFFF);
        assert_eq!(
            reason(read_entries(&bytes, EXTENDED)),
            (0x7FFF_FFFF, "its header does not fit the bytes read")
        );
    }

    #[test]
    fn a_chain_that_does_not_move_forward_is_refused() {
        let mut bytes = entry(EXTENDED, ADDED, 0, "first", false);
        bytes.extend(entry(EXTENDED, ADDED, 0, "second", true));
        // The next entry would start inside this one's header: a loop for a reader that trusts it.
        put_u32(&mut bytes, 0, 4);
        assert_eq!(reason(read_entries(&bytes, EXTENDED)), (0, "the next entry starts inside it"));
    }

    #[test]
    fn an_odd_name_length_is_refused() {
        let mut bytes = entry(BASIC, ADDED, 0, "ab", true);
        put_u32(&mut bytes, BASIC.name_length, 3);
        assert_eq!(reason(read_entries(&bytes, BASIC)), (0, "its name length is odd"));
    }
}
