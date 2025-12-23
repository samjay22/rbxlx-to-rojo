use crate::{filesystem::FileSystem, process_instructions, structures::*};
use log::info;
use pretty_assertions::assert_eq;
use rbx_dom_weak::types::Variant;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::ErrorKind,
    time::Instant,
};

#[derive(Deserialize, Serialize, Debug, PartialEq)]
enum VirtualFileContents {
    Bytes(String),
    Instance(HashMap<String, Variant>),
    Vfs(VirtualFileSystem),
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
struct VirtualFile {
    contents: VirtualFileContents,
}

#[derive(Deserialize, Serialize, Debug, Default)]
struct VirtualFileSystem {
    files: BTreeMap<String, VirtualFile>,
    tree: BTreeMap<String, TreePartition>,
    #[serde(skip)]
    finished: bool,
}

fn assert_vfs_contains(actual: &VirtualFileSystem, expected: &VirtualFileSystem, context: &str) {
    for (name, expected_file) in &expected.files {
        let actual_file = actual
            .files
            .get(name)
            .unwrap_or_else(|| panic!("missing file {} in {}", name, context));

        let next_context = if context.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", context, name)
        };

        assert_vfile_contains(actual_file, expected_file, &next_context);
    }

    for (name, expected_partition) in &expected.tree {
        let actual_partition = actual
            .tree
            .get(name)
            .unwrap_or_else(|| panic!("missing tree entry {} in {}", name, context));

        assert_eq!(actual_partition, expected_partition, "tree mismatch at {}", context);
    }
}

fn assert_vfile_contains(actual: &VirtualFile, expected: &VirtualFile, context: &str) {
    match (&actual.contents, &expected.contents) {
        (VirtualFileContents::Bytes(lhs), VirtualFileContents::Bytes(rhs)) => {
            assert_eq!(lhs, rhs, "byte content mismatch at {}", context)
        }
        (VirtualFileContents::Instance(lhs), VirtualFileContents::Instance(rhs)) => {
            assert_eq!(lhs, rhs, "instance content mismatch at {}", context)
        }
        (VirtualFileContents::Vfs(lhs), VirtualFileContents::Vfs(rhs)) => {
            assert_vfs_contains(lhs, rhs, context)
        }
        (lhs, rhs) => panic!(
            "type mismatch at {}: expected {:?} but found {:?}",
            context, rhs, lhs
        ),
    }
}

impl PartialEq<VirtualFileSystem> for VirtualFileSystem {
    fn eq(&self, rhs: &VirtualFileSystem) -> bool {
        self.files == rhs.files && self.tree == rhs.tree
    }
}

impl InstructionReader for VirtualFileSystem {
    fn finish_instructions(&mut self) {
        self.finished = true;
    }

    fn read_instruction<'a>(&mut self, instruction: Instruction<'a>) {
        match instruction {
            Instruction::AddToTree { name, partition } => {
                self.tree.insert(name, partition);
            }

            Instruction::CreateFile { filename, contents } => {
                let parent = filename
                    .parent()
                    .expect("no parent?")
                    .to_string_lossy()
                    .replace("\\", "/");
                let filename = filename
                    .file_name()
                    .expect("no filename?")
                    .to_string_lossy()
                    .replace("\\", "/");

                let system = if parent == "" {
                    self
                } else {
                    if !self.files.contains_key(&parent) {
                        self.files.insert(
                            parent.clone(),
                            VirtualFile {
                                contents: VirtualFileContents::Vfs(VirtualFileSystem::default()),
                            },
                        );
                    }

                    match self
                        .files
                        .get_mut(&parent)
                        .unwrap_or_else(|| panic!("no folder for {:?}", parent))
                        .contents
                    {
                        VirtualFileContents::Vfs(ref mut system) => system,
                        _ => unreachable!("attempt to parent to a file"),
                    }
                };

                let contents_string = String::from_utf8_lossy(&contents).into_owned();
                let rbxmx = filename.ends_with(".rbxmx");
                system.files.insert(
                    filename,
                    VirtualFile {
                        contents: if rbxmx {
                            let tree = rbx_xml::from_str_default(&contents_string)
                                .expect("couldn't decode encoded xml");
                            let child_id = tree.root().children()[0];
                            let child_instance = tree.get_by_ref(child_id).unwrap().clone();
                            VirtualFileContents::Instance(child_instance.properties.to_owned())
                        } else {
                            VirtualFileContents::Bytes(contents_string)
                        },
                    },
                );
            }

            Instruction::CreateFolder { folder } => {
                let name = folder.to_string_lossy().replace("\\", "/");
                self.files.insert(
                    name,
                    VirtualFile {
                        contents: VirtualFileContents::Vfs(VirtualFileSystem::default()),
                    },
                );
            }
        }
    }
}

#[test]
fn run_tests() {
    let _ = env_logger::init();
    for entry in fs::read_dir("./test-files").expect("couldn't read test-files") {
        let entry = entry.unwrap();
        let path = entry.path();
        info!("testing {:?}", path);

        let mut source_path = path.clone();
        source_path.push("source.rbxmx");
        let source = fs::read_to_string(&source_path).expect("couldn't read source.rbxmx");

        let time = Instant::now();
        let tree = rbx_xml::from_str_default(&source).expect("couldn't deserialize source.rbxmx");
        info!(
            "decoding for {:?} took {}ms",
            path,
            Instant::now().duration_since(time).as_millis()
        );

        let mut vfs = VirtualFileSystem::default();
        let time = Instant::now();
        process_instructions(&tree, &mut vfs);
        info!(
            "processing instructions for {:?} took {}ms",
            path,
            Instant::now().duration_since(time).as_millis()
        );

        let mut expected_path = path.clone();
        expected_path.push("output.json");
        assert!(vfs.finished, "finish_instructions was not called");

        if let Ok(expected) = fs::read_to_string(&expected_path) {
            let expected: VirtualFileSystem = serde_json::from_str(&expected).unwrap();
            assert_vfs_contains(&vfs, &expected, "");
        } else {
            let output = serde_json::to_string_pretty(&vfs).unwrap();
            fs::write(&expected_path, output).expect("couldn't write to output.json");
        }

        let filesystem_path = path.join("filesystem");
        if let Err(error) = fs::remove_dir_all(&filesystem_path) {
            match error.kind() {
                ErrorKind::NotFound => {}
                other => panic!("couldn't remove filesystem dir: {:?}", other),
            }
        }

        fs::create_dir(&filesystem_path).unwrap();

        let mut filesystem = FileSystem::from_root(filesystem_path);
        process_instructions(&tree, &mut filesystem);
    }
}
