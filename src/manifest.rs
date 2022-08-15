use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct DocserverInfo {
    pub git_commit: String,
}

#[derive(Deserialize)]
pub struct Manifest {
    pub package: Package,
    #[serde(default)]
    pub features: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub dependencies: HashMap<String, Dependency>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum DependencyEnum {
    Short(String),
    Full(Dependency),
}

pub struct Dependency {
    pub version: Option<String>,
    pub path: Option<String>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub optional: bool,
}

impl<'de> Deserialize<'de> for Dependency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(remote = "Dependency")] // cannot use `Self` here
        struct Full {
            #[serde(default)]
            version: Option<String>,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            git: Option<String>,
            #[serde(default)]
            rev: Option<String>,
            #[serde(default)]
            features: Vec<String>,
            #[serde(default)]
            no_default_features: bool,
            #[serde(default)]
            optional: bool,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ShortOrFull {
            Short(String),
            #[serde(with = "Full")]
            Full(Dependency),
        }

        Ok(match ShortOrFull::deserialize(deserializer)? {
            ShortOrFull::Short(version) => Self {
                version: Some(version),
                features: Vec::new(),
                no_default_features: false,
                optional: false,
                path: None,
                git: None,
                rev: None,
            },
            ShortOrFull::Full(this) => this,
        })
    }
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
    pub target: Option<String>,
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
    #[serde(default)]
    pub target: Option<String>,
}
