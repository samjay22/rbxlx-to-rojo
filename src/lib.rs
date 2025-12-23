use log::{debug, warn};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, WeakDom,
};
use rbx_reflection::ClassTag;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use structures::*;

pub mod filesystem;
pub mod structures;

#[cfg(test)]
mod tests;

lazy_static::lazy_static! {
    static ref NON_TREE_SERVICES: HashSet<&'static str> = include_str!("./non-tree-services.txt").lines().collect();
    #[allow(dead_code)]
    static ref RESPECTED_SERVICES: HashSet<&'static str> = include_str!("./respected-services.txt").lines().collect();
}

struct TreeIterator<'a, I: InstructionReader + ?Sized> {
    instruction_reader: &'a mut I,
    path: &'a Path,
    tree: &'a WeakDom,
}

const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn sanitize_component(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => c,
        })
        .collect();

    while sanitized.ends_with(' ') || sanitized.ends_with('.') {
        sanitized.pop();
    }

    if sanitized.is_empty() {
        sanitized.push('_');
    }

    let lower = sanitized.to_ascii_lowercase();
    if WINDOWS_RESERVED.contains(&lower.as_str()) {
        sanitized.insert(0, '_');
    }

    sanitized
}

fn sanitized_join(base: &Path, name: &str) -> PathBuf {
    base.join(sanitize_component(name))
}

fn repr_instance<'a>(
    base: &'a Path,
    child: &'a Instance,
    has_scripts: &'a HashMap<Ref, bool>,
) -> Option<(Vec<Instruction<'a>>, Cow<'a, Path>)> {
    match child.class.as_str() {
        "Folder" => {
            let folder_path = sanitized_join(base, &child.name);
            let owned: Cow<'a, Path> = Cow::Owned(folder_path);
            let clone = owned.clone();
            Some((
                vec![
                    Instruction::CreateFolder { folder: clone },
                    Instruction::CreateFile {
                        filename: Cow::Owned(owned.join("init.meta.json")),
                        contents: Cow::Owned(
                            serde_json::to_string_pretty(&MetaFile {
                                class_name: None,
                                // properties: BTreeMap::new(),
                                ignore_unknown_instances: true,
                            })
                            .unwrap()
                            .as_bytes()
                            .into(),
                        ),
                    },
                ],
                owned,
            ))
        }

        "Script" | "LocalScript" | "ModuleScript" => {
            let extension = match child.class.as_str() {
                "Script" => ".server",
                "LocalScript" => ".client",
                "ModuleScript" => "",
                _ => unreachable!(),
            };

            let source = match child.properties.get(&ustr::ustr("Source")) {
                Some(Variant::String(value)) => value.as_bytes(),
                Some(other) => {
                    warn!("unexpected Source variant for {} ( {:?} ), writing empty file", child.name, other);
                    &[]
                }
                None => {
                    warn!(
                        "missing Source on {} ({}), writing empty file",
                        child.name, child.class
                    );
                    &[]
                }
            };

            if child.children().is_empty() {
                Some((
                    vec![Instruction::CreateFile {
                        filename: Cow::Owned(sanitized_join(
                            base,
                            &format!("{}{}", child.name, extension),
                        )),
                        contents: Cow::Borrowed(source),
                    }],
                    Cow::Borrowed(base),
                ))
            } else {
                let meta_contents = Cow::Owned(
                    serde_json::to_string_pretty(&MetaFile {
                        class_name: None,
                        // properties: BTreeMap::new(),
                        ignore_unknown_instances: true,
                    })
                    .expect("couldn't serialize meta")
                    .as_bytes()
                    .into(),
                );

                let script_children_count = child
                    .children()
                    .iter()
                    .filter(|id| has_scripts.get(id) == Some(&true))
                    .count();

                let total_children_count = child.children().len();
                let folder_path: Cow<'a, Path> = Cow::Owned(sanitized_join(base, &child.name));

                // Any script with children becomes a folder so its descendants stay nested
                match script_children_count {
                    _ if script_children_count == total_children_count => Some((
                        vec![
                            Instruction::CreateFolder {
                                folder: folder_path.clone(),
                            },
                            Instruction::CreateFile {
                                filename: Cow::Owned(
                                    folder_path.join(format!("init{}.lua", extension)),
                                ),
                                contents: Cow::Borrowed(source),
                            },
                        ],
                        folder_path,
                    )),

                    0 => Some((
                        vec![
                            Instruction::CreateFolder {
                                folder: folder_path.clone(),
                            },
                            Instruction::CreateFile {
                                filename: Cow::Owned(
                                    folder_path.join(format!("init{}.lua", extension)),
                                ),
                                contents: Cow::Borrowed(source),
                            },
                            Instruction::CreateFile {
                                filename: Cow::Owned(folder_path.join("init.meta.json")),
                                contents: meta_contents,
                            },
                        ],
                        folder_path,
                    )),

                    _ => Some((
                        vec![
                            Instruction::CreateFolder {
                                folder: folder_path.clone(),
                            },
                            Instruction::CreateFile {
                                filename: Cow::Owned(
                                    folder_path.join(format!("init{}.lua", extension)),
                                ),
                                contents: Cow::Borrowed(source),
                            },
                            Instruction::CreateFile {
                                filename: Cow::Owned(folder_path.join("init.meta.json")),
                                contents: meta_contents,
                            },
                        ],
                        folder_path,
                    )),
                }
            }
        }

        other_class => {
            // When all else fails, represent the instance with a meta folder so it isn't lost
            let db = rbx_reflection_database::get().expect("couldn't get reflection database");
            match db.classes.get(other_class) {
                Some(reflected) => {
                    let is_service = reflected.tags.contains(&ClassTag::Service);
                    if is_service {
                        let new_base: Cow<'a, Path> = Cow::Owned(sanitized_join(base, &child.name));
                        let mut instructions = Vec::new();

                        if !NON_TREE_SERVICES.contains(other_class) {
                            instructions
                                .push(Instruction::add_to_tree(&child, new_base.to_path_buf()));
                        }

                        instructions.push(Instruction::CreateFolder {
                            folder: new_base.clone(),
                        });

                        return Some((instructions, new_base));
                    }
                }

                None => {
                    debug!("class is not in reflection? {}", other_class);
                }
            }

            // Represent the instance using a .meta.json folder so it persists in the project
            let folder_path: Cow<'a, Path> = Cow::Owned(sanitized_join(base, &child.name));
            let meta = MetaFile {
                class_name: Some(child.class.to_string()),
                // properties: properties.into_iter().collect(),
                ignore_unknown_instances: true,
            };

            Some((
                vec![
                    Instruction::CreateFolder {
                        folder: folder_path.clone(),
                    },
                    Instruction::CreateFile {
                        filename: Cow::Owned(folder_path.join("init.meta.json")),
                        contents: Cow::Owned(
                            serde_json::to_string_pretty(&meta)
                                .expect("couldn't serialize meta")
                                .as_bytes()
                                .into(),
                        ),
                    },
                ],
                folder_path,
            ))
        }
    }
}

impl<'a, I: InstructionReader + ?Sized> TreeIterator<'a, I> {
    fn visit_instructions(&mut self, instance: &Instance, has_scripts: &HashMap<Ref, bool>) {
        for child_id in instance.children() {
            let child = self.tree.get_by_ref(*child_id).expect("got fake child id?");

            let (instructions_to_create_base, path) = if child.class == "StarterPlayer" {
                // We can't respect StarterPlayer as a service, because then Rojo
                // tries to delete StarterPlayerScripts and whatnot, which is not valid.
                let folder_path: Cow<'a, Path> = Cow::Owned(sanitized_join(self.path, &child.name));
                let mut instructions = Vec::new();

                instructions.push(Instruction::CreateFolder {
                    folder: folder_path.clone(),
                });

                instructions.push(Instruction::AddToTree {
                    name: child.name.to_string(),
                    partition: TreePartition {
                        class_name: child.class.to_string(),
                        children: child
                            .children()
                            .iter()
                            .map(|child_id| {
                                let child = self.tree.get_by_ref(*child_id).unwrap();
                                (
                                    child.name.to_string(),
                                        Instruction::partition(
                                            &child,
                                            sanitized_join(folder_path.as_ref(), child.name.as_str()),
                                        ),
                                )
                            })
                            .collect(),
                        ignore_unknown_instances: true,
                        path: None,
                    },
                });

                (instructions, folder_path)
            } else {
                match repr_instance(&self.path, child, has_scripts) {
                    Some((instructions_to_create_base, path)) => {
                        (instructions_to_create_base, path)
                    }
                    None => continue,
                }
            };

            self.instruction_reader
                .read_instructions(instructions_to_create_base);

            TreeIterator {
                instruction_reader: self.instruction_reader,
                path: &path,
                tree: self.tree,
            }
            .visit_instructions(child, has_scripts);
        }
    }
}

fn check_has_scripts(
    tree: &WeakDom,
    instance: &Instance,
    has_scripts: &mut HashMap<Ref, bool>,
) -> bool {
    let mut children_have_scripts = false;

    for child_id in instance.children() {
        let result = check_has_scripts(
            tree,
            tree.get_by_ref(*child_id).expect("fake child id?"),
            has_scripts,
        );

        children_have_scripts = children_have_scripts || result;
    }

    let result = match instance.class.as_str() {
        "Script" | "LocalScript" | "ModuleScript" => true,
        _ => children_have_scripts,
    };

    has_scripts.insert(instance.referent(), result);
    result
}

pub fn process_instructions(tree: &WeakDom, instruction_reader: &mut dyn InstructionReader) {
    let root = tree.root_ref();
    let root_instance = tree.get_by_ref(root).expect("fake root id?");
    let path = PathBuf::new();

    let mut has_scripts = HashMap::new();
    check_has_scripts(tree, root_instance, &mut has_scripts);

    TreeIterator {
        instruction_reader,
        path: &path,
        tree,
    }
    .visit_instructions(&root_instance, &has_scripts);

    instruction_reader.finish_instructions();
}
