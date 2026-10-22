//! BitTorrent v2 file tree structure (BEP-52).

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::error::MetainfoError;
use crate::bencode::Value;

/// A file entry in a v2 file tree.
#[derive(Debug, Clone)]
pub struct FileTreeEntry {
    /// The length of the file in bytes.
    pub length: u64,
    /// The root hash of the file's merkle tree (32 bytes).
    pub pieces_root: Option<[u8; 32]>,
    /// File attributes (e.g., "p" for padding, "x" for executable, "h" for hidden).
    pub attr: Option<String>,
}

/// Hierarchical file structure for BitTorrent v2 torrents (BEP-52).
///
/// In v2 torrents, files are organized in a tree structure where directories
/// are represented as nested dictionaries and files are leaf nodes.
#[derive(Debug, Clone)]
pub enum FileTree {
    /// A file node with length and optional pieces root.
    File(FileTreeEntry),
    /// A directory node containing child entries.
    Directory(BTreeMap<String, FileTree>),
}

/// Flattened file from a file tree.
#[derive(Debug, Clone)]
pub struct FlattenedFile {
    /// Full path to the file.
    pub path: PathBuf,
    /// File length in bytes.
    pub length: u64,
    /// Pieces root hash (32 bytes) for v2 verification.
    pub pieces_root: Option<[u8; 32]>,
    /// File attributes (e.g., "p" for padding, "x" for executable, "h" for hidden).
    pub attr: Option<String>,
}

impl FileTree {
    /// Parses a file tree from a bencoded value.
    ///
    /// The value should be the `file tree` dictionary from a v2 torrent's info dict.
    pub fn from_bencode(value: &Value) -> Result<Self, MetainfoError> {
        parse_file_tree_node(value)
    }

    /// Flattens the file tree into a list of files with full paths.
    ///
    /// Directory structure is converted to path components.
    pub fn flatten(&self) -> Vec<FlattenedFile> {
        let mut files = Vec::new();
        flatten_recursive(self, PathBuf::new(), &mut files);
        files
    }

    /// Returns true if this is a file node.
    pub fn is_file(&self) -> bool {
        matches!(self, FileTree::File(_))
    }

    /// Returns true if this is a directory node.
    pub fn is_directory(&self) -> bool {
        matches!(self, FileTree::Directory(_))
    }

    /// Returns the file entry if this is a file node.
    pub fn as_file(&self) -> Option<&FileTreeEntry> {
        match self {
            FileTree::File(entry) => Some(entry),
            FileTree::Directory(_) => None,
        }
    }

    /// Returns the directory contents if this is a directory node.
    pub fn as_directory(&self) -> Option<&BTreeMap<String, FileTree>> {
        match self {
            FileTree::File(_) => None,
            FileTree::Directory(children) => Some(children),
        }
    }
}

fn parse_file_tree_node(value: &Value) -> Result<FileTree, MetainfoError> {
    let dict = value
        .as_dict()
        .ok_or(MetainfoError::InvalidField("file tree"))?;

    // Check if this is a file node (has empty string key with length)
    if let Some(file_info) = dict.get(b"".as_slice()) {
        let file_dict = file_info
            .as_dict()
            .ok_or(MetainfoError::InvalidField("file tree entry"))?;

        let length = file_dict
            .get(b"length".as_slice())
            .and_then(|v| v.as_integer())
            .ok_or(MetainfoError::MissingField("length"))? as u64;

        let pieces_root = file_dict
            .get(b"pieces root".as_slice())
            .and_then(|v| v.as_bytes())
            .and_then(|b| {
                if b.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(b);
                    Some(arr)
                } else {
                    None
                }
            });

        let attr = file_dict
            .get(b"attr".as_slice())
            .and_then(|v| v.as_str())
            .map(String::from);

        return Ok(FileTree::File(FileTreeEntry {
            length,
            pieces_root,
            attr,
        }));
    }

    // Otherwise, this is a directory node
    let mut children = BTreeMap::new();
    for (key, value) in dict {
        let name = std::str::from_utf8(key)
            .map_err(|_| MetainfoError::InvalidField("file tree key"))?
            .to_string();

        // Skip empty keys (handled above)
        if name.is_empty() {
            continue;
        }

        let child = parse_file_tree_node(value)?;
        children.insert(name, child);
    }

    Ok(FileTree::Directory(children))
}

fn flatten_recursive(tree: &FileTree, current_path: PathBuf, files: &mut Vec<FlattenedFile>) {
    match tree {
        FileTree::File(entry) => {
            files.push(FlattenedFile {
                path: current_path,
                length: entry.length,
                pieces_root: entry.pieces_root,
                attr: entry.attr.clone(),
            });
        }
        FileTree::Directory(children) => {
            for (name, child) in children {
                let child_path = current_path.join(name);
                flatten_recursive(child, child_path, files);
            }
        }
    }
}
