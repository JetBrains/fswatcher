use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, PartialEq)]
pub enum TreeLike<Key, Content>
where
    Key: Eq + Hash,
{
    Leaf(Content),
    Node { children: HashMap<Key, TreeLike<Key, Content>> },
}

impl<Key, Content> TreeLike<Key, Content>
where
    Key: Eq + Hash,
{
    pub fn empty() -> Self {
        TreeLike::Node { children: HashMap::new() }
    }
}

macro_rules! tree_like {
    ( {} ) => {
        $crate::test_helpers::tree_like::TreeLike::empty()
    };
    ( { $( $child_name:expr => $content:tt ),* $(,)? } ) => {
        $crate::test_helpers::tree_like::TreeLike::Node {
            children: {
                #[allow(unused_mut)]
                let mut map = std::collections::HashMap::new();
                $( map.insert($child_name.into(), $crate::test_helpers::tree_like!($content)); )*
                map
            }
        }
    };
    ( $( $child_name:expr => $content:tt ),* $(,)? ) => {
        $crate::test_helpers::tree_like::TreeLike::Node {
            children: {
                #[allow(unused_mut)]
                let mut map = std::collections::HashMap::new();
                $( map.insert($child_name.into(), $crate::test_helpers::tree_like!($content)); )*
                map
            }
        }
    };
    ( $content:expr ) => { $crate::test_helpers::tree_like::TreeLike::Leaf($content.into()) };
}

pub(crate) use tree_like;
