use rustwide::{Crate, GitRegistry};

const INDEX_URL: &str = "https://github.com/rust-lang/staging.crates.io-index";

#[test]
fn legacy_registry_names_remain_compatible() {
    let mut registry = rustwide::AlternativeRegistry::new(INDEX_URL);
    registry.authenticate_with_ssh_key("unused test key");
    let registry: GitRegistry = registry;
    let legacy = Crate::registry(registry, "gcc", "0.3.38");
    let current = Crate::git_registry(GitRegistry::new(INDEX_URL), "gcc", "0.3.38");
    assert_eq!(legacy.to_string(), current.to_string());
}

#[test]
fn test_fetch() -> anyhow::Result<()> {
    let workspace = crate::utils::init_workspace()?;

    let registry = GitRegistry::new(INDEX_URL);
    let krate = Crate::git_registry(registry, "gcc", "0.3.38");
    krate.fetch(&workspace)?;

    Ok(())
}
