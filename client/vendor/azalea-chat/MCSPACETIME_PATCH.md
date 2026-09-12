# vendored azalea-chat 0.10.3+mc1.21.1

Copied from crates.io. One change: the two `Serialize` impls used
`serde::__private::ser::FlatMapSerializer`, an internal serde item removed in newer serde
releases; they now nest the base component under a "base" key instead of flattening it.
We never serialise chat components to JSON, so the output difference is irrelevant.
