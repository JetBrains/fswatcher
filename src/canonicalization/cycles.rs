use std::path::Path;

use super::CanonicalKey;

pub struct CycleDetection {
    // visited_symlinks: HashMap<CanonicalKey, usize>,
    quota: usize,
}

impl CycleDetection {
    pub fn new() -> Self {
        CycleDetection {
            // visited_symlinks: HashMap::new(),
            quota: 128,
        }
    }

    /// Returns false if in a cycle.
    #[must_use]
    pub fn record_visit(&mut self, _symlink_key: CanonicalKey, _current_suffix: &Path) -> bool {
        if self.quota == 0 {
            return false;
        }
        self.quota -= 1;
        return true;

        // trace!(?symlink_key, ?current_suffix, "record_visit");
        // let tail_length = current_suffix.components().count();
        // // it is okay to visit the same symlink twice as long as we are making progress towards the end of the path
        // self.visited_symlinks
        //     .insert(symlink_key, tail_length)
        //     .map(|previous_tail| tail_length < previous_tail)
        //     .unwrap_or(true)
    }
}

#[cfg(test)]
mod test {
    // use crate::util::minislab::{SlabKey, SlabKeyType};
    //
    // use super::*;
    //
    // fn key(u: usize) -> CanonicalKey {
    //     SlabKeyType::from(SlabKey::from_usize(u))
    // }
    //
    // #[test]
    // fn progress() {
    //     // /d/s/s/f where
    //     // s => ../d
    //     let mut c = CycleDetection::new();
    //     assert!(c.record_visit(key(0), Path::new("s/f")));
    //     assert!(c.record_visit(key(0), Path::new("f")));
    // }
    //
    // #[test]
    // fn cycle() {
    //     // /s/f where
    //     // s => s
    //     let mut c = CycleDetection::new();
    //     assert!(c.record_visit(key(0), Path::new("f")));
    //     assert!(!c.record_visit(key(0), Path::new("f")));
    // }
    //
    // #[test]
    // fn growing_cycle() {
    //     // /s/f where
    //     // s => s/d
    //     let mut c = CycleDetection::new();
    //     assert!(c.record_visit(key(0), Path::new("f")));
    //     assert!(!c.record_visit(key(0), Path::new("d/f")));
    // }
    //
    // #[test]
    // fn same_link_in_two_components() {
    //     let mut c = CycleDetection::new();
    //     // /s1/s2/f where
    //     // s1 => /1,
    //     // s2 => /s1/2
    //     assert!(c.record_visit(key(1), Path::new("s2/f")));
    //     assert!(c.record_visit(key(2), Path::new("f")));
    //     assert!(c.record_visit(key(1), Path::new("2/f")));
    // }
}
