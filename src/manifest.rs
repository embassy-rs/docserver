use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub features: HashMap<String, Vec<String>>,
    pub package: Package,
}

#[derive(Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Deserialize, Default)]
pub struct Metadata {
    #[serde(default)]
    pub embassy_docs: Docs,
}

#[derive(Deserialize, Default)]
pub struct Docs {
    #[serde(default)]
    pub flavors: Vec<DocsFlavor>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub src_base: String,
    #[serde(default)]
    pub src_base_git: String,
}

#[derive(Deserialize)]
pub struct DocsFlavor {
    // One of either has to be specified
    pub regex_feature: Option<String>,
    pub name: Option<String>,

    #[serde(default)]
    pub features: Vec<String>,
    pub target: String,
}
