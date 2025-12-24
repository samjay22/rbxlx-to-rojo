use log::{debug, warn};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, InstanceBuilder, WeakDom,
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

#[derive(Clone, Copy)]
pub enum ExportMode {
    Full,
    ScriptsOnly,
}

struct TreeIterator<'a, I: InstructionReader + ?Sized> {
    instruction_reader: &'a mut I,
    path: &'a Path,
    tree: &'a WeakDom,
    mode: ExportMode,
}

#[derive(Clone, Copy)]
enum ChildTraversal {
    Normal,
    ScriptsOnly,
    Skip,
}

struct Representation<'a> {
    instructions: Vec<Instruction<'a>>,
    path: Cow<'a, Path>,
    traversal: ChildTraversal,
}

const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn is_script_class(class_name: &str) -> bool {
    matches!(class_name, "Script" | "LocalScript" | "ModuleScript")
}

fn should_skip_service(class_name: &str) -> bool {
    let db = match rbx_reflection_database::get() {
        Ok(db) => db,
        Err(error) => {
            warn!("couldn't get reflection database: {:?}", error);
            return false;
        }
    };

    match db.classes.get(class_name) {
        Some(reflected) => reflected.tags.contains(&ClassTag::Service) && !RESPECTED_SERVICES.contains(class_name),
        None => false,
    }
}

fn sanitize_component(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
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

fn clone_without_scripts(
    source: &WeakDom,
    source_ref: Ref,
    target: &mut WeakDom,
    parent: Ref,
) -> Option<Ref> {
    let instance = source.get_by_ref(source_ref)?;

    if is_script_class(instance.class.as_str()) {
        return None;
    }

    let mut builder = InstanceBuilder::new(instance.class.clone()).with_name(instance.name.clone());

    for (key, value) in instance.properties.iter() {
        builder = builder.with_property(key.clone(), value.clone());
    }

    let new_ref = target.insert(parent, builder);

    for child_ref in instance.children() {
        clone_without_scripts(source, *child_ref, target, new_ref);
    }

    Some(new_ref)
}

fn serialize_instance_to_rbxm(tree: &WeakDom, instance: &Instance) -> Option<Vec<u8>> {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("DataModel"));
    let dom_root = dom.root_ref();

    let Some(root_ref) = clone_without_scripts(tree, instance.referent(), &mut dom, dom_root) else {
        return None;
    };

    let mut bytes = Vec::new();

    match rbx_xml::to_writer_default(&mut bytes, &dom, &[root_ref]) {
        Ok(()) => Some(bytes),
        Err(error) => {
            warn!("couldn't serialize {} to rbxm: {:?}", instance.name, error);
            None
        }
    }
}
fn repr_instance<'a>(
    tree: &'a WeakDom,
    base: &'a Path,
    child: &'a Instance,
    has_scripts: &'a HashMap<Ref, bool>,
    mode: ExportMode,
) -> Option<Representation<'a>> {
    let contains_scripts = has_scripts.get(&child.referent()).copied().unwrap_or(false);

    match child.class.as_str() {
        "Folder" => {
            if matches!(mode, ExportMode::ScriptsOnly) && !contains_scripts {
                return None;
            }

            let folder_path = sanitized_join(base, &child.name);
            let owned: Cow<'a, Path> = Cow::Owned(folder_path);
            let clone = owned.clone();
            Some(Representation {
                instructions: vec![
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
                path: owned,
                traversal: ChildTraversal::Normal,
            })
        }

        "Script" | "LocalScript" | "ModuleScript" => {
            let extension = match child.class.as_str() {
                "Script" => ".server.luau",
                "LocalScript" => ".client.luau",
                "ModuleScript" => ".luau",
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
                Some(Representation {
                    instructions: vec![Instruction::CreateFile {
                        filename: Cow::Owned(sanitized_join(
                            base,
                            &format!("{}{}", child.name, extension),
                        )),
                        contents: Cow::Borrowed(source),
                    }],
                    path: Cow::Borrowed(base),
                    traversal: ChildTraversal::Skip,
                })
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
                    _ if script_children_count == total_children_count => Some(Representation {
                        instructions: vec![
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
                        path: folder_path,
                        traversal: ChildTraversal::Normal,
                    }),

                    0 => Some(Representation {
                        instructions: vec![
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
                        path: folder_path,
                        traversal: ChildTraversal::Normal,
                    }),

                    _ => Some(Representation {
                        instructions: vec![
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
                        path: folder_path,
                        traversal: ChildTraversal::Normal,
                    }),
                }
            }
        }

        other_class => {
            let db = rbx_reflection_database::get().expect("couldn't get reflection database");
            match db.classes.get(other_class) {
                Some(reflected) => {
                    let is_service = reflected.tags.contains(&ClassTag::Service);
                    if is_service {
                        if matches!(mode, ExportMode::ScriptsOnly) && !contains_scripts {
                            return None;
                        }

                        if !RESPECTED_SERVICES.contains(other_class) {
                            return None;
                        }

                        let new_base: Cow<'a, Path> = Cow::Owned(sanitized_join(base, &child.name));
                        let mut instructions = Vec::new();

                        if !NON_TREE_SERVICES.contains(other_class) {
                            instructions
                                .push(Instruction::add_to_tree(&child, new_base.to_path_buf()));
                        }

                        instructions.push(Instruction::CreateFolder {
                            folder: new_base.clone(),
                        });

                        return Some(Representation {
                            instructions,
                            path: new_base,
                            traversal: ChildTraversal::Normal,
                        });
                    }
                }

                None => {
                    debug!("class is not in reflection? {}", other_class);
                }
            }

            if matches!(mode, ExportMode::ScriptsOnly) && !contains_scripts {
                return None;
            }

            let folder_path: Cow<'a, Path> = Cow::Owned(sanitized_join(base, &child.name));

            let mut instructions = vec![Instruction::CreateFolder {
                folder: folder_path.clone(),
            }];

            let traversal = if contains_scripts {
                if matches!(mode, ExportMode::ScriptsOnly) {
                    ChildTraversal::ScriptsOnly
                } else {
                    let model_bytes = serialize_instance_to_rbxm(tree, child)?;
                    instructions.push(Instruction::CreateFile {
                        filename: Cow::Owned(folder_path.join("init.rbxmx")),
                        contents: Cow::Owned(model_bytes),
                    });
                    ChildTraversal::ScriptsOnly
                }
            } else {
                ChildTraversal::Skip
            };

            Some(Representation {
                instructions,
                path: folder_path,
                traversal,
            })
        }
    }
}

impl<'a, I: InstructionReader + ?Sized> TreeIterator<'a, I> {
    fn visit_instructions(
        &mut self,
        instance: &Instance,
        has_scripts: &HashMap<Ref, bool>,
        scripts_only: bool,
    ) {
        for child_id in instance.children() {
            let child = self.tree.get_by_ref(*child_id).expect("got fake child id?");

            if matches!(self.mode, ExportMode::ScriptsOnly) && !has_scripts.get(child_id).copied().unwrap_or(false) {
                continue;
            }

            if scripts_only && !is_script_class(child.class.as_str()) {
                if *has_scripts.get(child_id).unwrap_or(&false) {
                    let next_path = sanitized_join(self.path, &child.name);

                    TreeIterator {
                        instruction_reader: self.instruction_reader,
                        path: next_path.as_path(),
                        tree: self.tree,
                        mode: self.mode,
                    }
                    .visit_instructions(child, has_scripts, true);
                }

                continue;
            }

            if should_skip_service(child.class.as_str()) {
                continue;
            }

            let representation = if child.class == "StarterPlayer" {
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

                Some(Representation {
                    instructions,
                    path: folder_path,
                    traversal: ChildTraversal::Normal,
                })
            } else {
                repr_instance(self.tree, self.path, child, has_scripts, self.mode)
            };

            let Some(representation) = representation else {
                continue;
            };

            let Representation {
                instructions,
                path,
                traversal,
            } = representation;

            self.instruction_reader.read_instructions(instructions);

            let mut iterator = TreeIterator {
                instruction_reader: self.instruction_reader,
                path: path.as_ref(),
                tree: self.tree,
                mode: self.mode,
            };

            match traversal {
                ChildTraversal::Normal => iterator.visit_instructions(child, has_scripts, scripts_only),
                ChildTraversal::ScriptsOnly => iterator.visit_instructions(child, has_scripts, true),
                ChildTraversal::Skip => {}
            }
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

pub fn process_instructions(
    tree: &WeakDom,
    instruction_reader: &mut dyn InstructionReader,
    mode: ExportMode,
) {
    let root = tree.root_ref();
    let root_instance = tree.get_by_ref(root).expect("fake root id?");
    let path = PathBuf::new();

    let mut has_scripts = HashMap::new();
    check_has_scripts(tree, root_instance, &mut has_scripts);

    TreeIterator {
        instruction_reader,
        path: &path,
        tree,
        mode,
    }
    .visit_instructions(&root_instance, &has_scripts, false);

    instruction_reader.finish_instructions();
}
