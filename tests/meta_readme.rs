#![cfg(not(miri))]

#[test]
#[ignore = "This isn't read at all."]
fn installation() {
	version_sync::assert_contains_regex!("README.md", "^cargo install {name}$");
}

#[test]
fn versioning() {
	version_sync::assert_contains_regex!(
		"README.md",
		r"^`{name}` strictly follows \[Semantic Versioning 2\.0\.0\]"
	);
}
