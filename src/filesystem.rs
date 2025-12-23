use crate::structures::*;
use serde::{ser::SerializeMap, Serialize, Serializer};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

const SRC: &str = "src";

fn serialize_project_tree<S: Serializer>(
    tree: &BTreeMap<String, TreePartition>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(Some(tree.len() + 1))?;
    map.serialize_entry("$className", "DataModel")?;
    for (k, v) in tree {
        map.serialize_entry(k, v)?;
    }
    map.end()
}

#[derive(Clone, Debug, Serialize)]
struct Project {
    name: String,
    #[serde(serialize_with = "serialize_project_tree")]
    tree: BTreeMap<String, TreePartition>,
}

impl Project {
    fn new() -> Self {
        Self {
            name: "project".to_string(),
            tree: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileSystem {
    project: Project,
    root: PathBuf,
    source: PathBuf,
}

impl FileSystem {
    pub fn from_root(root: PathBuf) -> Self {
        let source = root.join(SRC);
        let project = Project::new();

        fs::create_dir(&source).ok(); // It'll error later if it matters

        Self {
            project,
            root,
            source,
        }
    }
}

impl InstructionReader for FileSystem {
    fn read_instruction<'a>(&mut self, instruction: Instruction<'a>) {
        match instruction {
            Instruction::AddToTree {
                mut name,
                mut partition,
            } => {
                if self.project.tree.contains_key(&name) {
                    let original = name.clone();
                    let mut counter = 2;
                    loop {
                        let candidate = format!("{}_{}", original, counter);
                        if !self.project.tree.contains_key(&candidate) {
                            name = candidate;
                            break;
                        }
                        counter += 1;
                    }

                    if let Some(path) = partition.path.take() {
                        let new_path = match path.parent() {
                            Some(parent) => parent.join(&name),
                            None => PathBuf::from(&name),
                        };
                        partition.path = Some(new_path);
                    }
                }

                if let Some(path) = partition.path {
                    partition.path = Some(PathBuf::from(SRC).join(path));
                }

                for child in partition.children.values_mut() {
                    if let Some(path) = &child.path {
                        child.path = Some(PathBuf::from(SRC).join(path));
                    }
                }

                self.project.tree.insert(name, partition);
            }

            Instruction::CreateFile { filename, contents } => {
                let full_path = self.source.join(&filename);

                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).unwrap_or_else(|error| {
                        panic!("can't create parent dirs for {:?}: {:?}", full_path, error)
                    });
                }

                let mut file = File::create(&full_path).unwrap_or_else(|error| {
                    panic!("can't create file {:?}: {:?}", full_path, error)
                });
                file.write_all(&contents).unwrap_or_else(|error| {
                    panic!("can't write to file {:?} due to {:?}", filename, error)
                });
            }

            Instruction::CreateFolder { folder } => {
                fs::create_dir_all(self.source.join(&folder)).unwrap_or_else(|error| {
                    panic!("can't write to folder {:?}: {:?}", folder, error)
                });
            }
        }
    }

    fn finish_instructions(&mut self) {
        let mut file = File::create(self.root.join("default.project.json"))
            .expect("can't create default.project.json");
        file.write_all(
            &serde_json::to_string_pretty(&self.project)
                .expect("couldn't serialize project")
                .as_bytes(),
        )
        .expect("can't write project");
    }
}
