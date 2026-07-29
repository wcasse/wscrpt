use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Component, Path, PathBuf};

use crate::editor::EditorState;
use crate::{Document, DocumentError, Editor};

const ROOT_MARKERS: &[&str] = &[
    ".git",
    ".hg",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "Makefile",
];

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    buffers: Vec<Editor>,
    /// Editor IDs are process-local, trusted integer keys. Keep exactly one
    /// entry per buffer so request routing does not scan every open editor.
    editor_indices: HashMap<u64, usize>,
    active: usize,
}

/// Mutable access to one editor's contents without exposing its stable ID.
/// The guard owns no cleanup responsibility, so even safely forgetting it
/// cannot leave the workspace's editor index stale.
pub struct EditorMut<'a> {
    state: &'a mut EditorState,
}

impl Deref for EditorMut<'_> {
    type Target = EditorState;

    fn deref(&self) -> &Self::Target {
        self.state
    }
}

impl DerefMut for EditorMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state
    }
}

impl Drop for EditorMut<'_> {
    fn drop(&mut self) {
        // Identity and index reconciliation are deliberately unnecessary:
        // this guard can mutate only EditorState. Keep an explicit Drop so
        // callers may end the guard's borrow with `drop(guard)` and tests can
        // adversarially skip that path with `mem::forget`.
    }
}

impl Workspace {
    pub fn new(root: Option<PathBuf>) -> io::Result<Self> {
        let root = root
            .map(absolute_path)
            .transpose()?
            .unwrap_or(env::current_dir()?);
        Ok(Self::with_editor(root, Editor::new(Document::new())))
    }

    pub fn from_path(path: Option<PathBuf>, root: Option<PathBuf>) -> Result<Self, DocumentError> {
        let cwd = env::current_dir().map_err(|source| DocumentError::Read {
            path: PathBuf::from("."),
            source,
        })?;
        let requested = path.map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        });
        let inferred_from = requested
            .as_deref()
            .and_then(|path| path.parent())
            .unwrap_or(&cwd);
        let root = root
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                }
            })
            .unwrap_or_else(|| discover_root(inferred_from));
        let document = match requested {
            Some(path) if path.exists() => Document::open(path)?,
            Some(path) => Document::new_at(path),
            None => Document::new(),
        };
        Ok(Self::with_editor(root, Editor::new(document)))
    }

    pub fn active(&self) -> &Editor {
        &self.buffers[self.active]
    }

    pub fn active_mut(&mut self) -> EditorMut<'_> {
        self.editor_mut(self.active)
            .expect("the active workspace buffer exists")
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn buffers(&self) -> &[Editor] {
        &self.buffers
    }

    /// Mutably iterate documents without exposing editor IDs or buffer order.
    /// There is intentionally no mutable editor-slice API that could reorder
    /// stable IDs behind the workspace index.
    ///
    /// ```compile_fail
    /// let mut workspace = wscrpt::Workspace::new(None).unwrap();
    /// workspace.buffers_mut().swap(0, 1);
    /// ```
    pub fn documents_mut(&mut self) -> impl Iterator<Item = &mut Document> {
        self.buffers.iter_mut().map(|editor| &mut editor.document)
    }

    /// Mutably access one editor's state without exposing a reorderable slice
    /// or its stable workspace identity. Whole-editor replacement must go
    /// through [`Workspace::replace_editor`].
    ///
    /// ```compile_fail
    /// let mut workspace = wscrpt::Workspace::new(None).unwrap();
    /// let mut state = workspace.active_mut();
    /// *state = wscrpt::Editor::new(wscrpt::Document::new());
    /// ```
    pub fn editor_mut(&mut self, index: usize) -> Option<EditorMut<'_>> {
        let editor = self.buffers.get_mut(index)?;
        Some(EditorMut { state: editor })
    }

    /// Return the stable index for an editor ID without changing which buffer
    /// is active.
    pub fn editor_index(&self, editor_id: u64) -> Option<usize> {
        let index = *self.editor_indices.get(&editor_id)?;
        self.buffers
            .get(index)
            .is_some_and(|editor| editor.id() == editor_id)
            .then_some(index)
    }

    /// Look up any editor, including an untitled or virtual buffer, by its
    /// stable ID without changing which buffer is active.
    pub fn editor_by_id(&self, editor_id: u64) -> Option<&Editor> {
        self.editor_index(editor_id)
            .map(|index| &self.buffers[index])
    }

    /// Mutably look up one editor by its stable ID without changing which
    /// buffer is active.
    pub fn editor_by_id_mut(&mut self, editor_id: u64) -> Option<EditorMut<'_>> {
        let index = self.editor_index(editor_id)?;
        self.editor_mut(index)
    }

    /// Replace one editor without changing its position or active-buffer
    /// semantics. Callers that need to replace a whole `Editor` must use this
    /// instead of assigning through a mutable-state guard, because a
    /// replacement gets a new stable editor ID. Returns `None` for an invalid
    /// index or an editor ID already owned by another buffer.
    pub fn replace_editor(&mut self, index: usize, editor: Editor) -> Option<Editor> {
        let old_id = self.buffers.get(index)?.id();
        if editor.id() != old_id && self.editor_indices.contains_key(&editor.id()) {
            return None;
        }
        let new_id = editor.id();
        let previous = std::mem::replace(&mut self.buffers[index], editor);
        self.editor_indices.remove(&old_id);
        self.editor_indices.insert(new_id, index);
        debug_assert!(self.indices_are_consistent());
        Some(previous)
    }

    /// Look up a file-backed editor by path without changing which buffer is
    /// active. Relative paths are resolved from the workspace root. Existing
    /// aliases are canonicalized; nonexistent paths are normalized against
    /// their nearest existing ancestor.
    pub fn editor_by_path(&self, path: impl AsRef<Path>) -> Option<&Editor> {
        let identity = self.file_identity(path.as_ref());
        self.buffers.iter().find(|editor| {
            editor
                .document
                .path()
                .is_some_and(|open_path| self.file_identity(open_path) == identity)
        })
    }

    /// Mutably look up one file-backed editor by path without changing which
    /// buffer is active.
    pub fn editor_by_path_mut(&mut self, path: impl AsRef<Path>) -> Option<EditorMut<'_>> {
        let identity = self.file_identity(path.as_ref());
        let index = self.buffers.iter().position(|editor| {
            editor
                .document
                .path()
                .is_some_and(|open_path| self.file_identity(open_path) == identity)
        })?;
        self.editor_mut(index)
    }

    /// Iterate over file-backed editors only. The iterator is immutable so a
    /// caller can snapshot IDs and document metadata before individually
    /// reconciling buffers through `editor_by_id_mut`.
    pub fn file_editors(&self) -> impl Iterator<Item = &Editor> {
        self.buffers
            .iter()
            .filter(|editor| editor.document.path().is_some())
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    pub fn open(&mut self, path: impl AsRef<Path>) -> Result<usize, DocumentError> {
        let path = self.resolve(path.as_ref());
        let identity = normalized_file_path(&path);
        if let Some(index) = self.buffers.iter().position(|editor| {
            editor
                .document
                .path()
                .is_some_and(|open_path| self.file_identity(open_path) == identity)
        }) {
            self.active = index;
            return Ok(index);
        }

        let document = if path.exists() {
            Document::open(&path)?
        } else {
            Document::new_at(&path)
        };
        if self.buffers.len() == 1
            && self.buffers[0].document.path().is_none()
            && !self.buffers[0].document.is_modified()
        {
            let _replaced = self
                .replace_editor(0, Editor::new(document))
                .expect("the initial workspace buffer exists");
            self.active = 0;
        } else {
            self.active = self.push_editor(Editor::new(document));
        }
        Ok(self.active)
    }

    pub fn new_buffer(&mut self) -> usize {
        self.active = self.push_editor(Editor::new(Document::new()));
        self.active
    }

    pub fn open_virtual(&mut self, name: impl Into<String>, text: &str) -> usize {
        let name = name.into();
        if let Some(index) = self.buffers.iter().position(|editor| {
            editor.document.is_read_only() && editor.document.display_name() == name
        }) {
            let _replaced = self
                .replace_editor(index, Editor::new(Document::virtual_view(name, text)))
                .expect("the matching virtual buffer exists");
            self.active = index;
            return index;
        }
        self.active = self.push_editor(Editor::new(Document::virtual_view(name, text)));
        self.active
    }

    pub fn update_virtual(&mut self, name: &str, text: &str, follow_end: bool) -> bool {
        let Some(index) = self.buffers.iter().position(|editor| {
            editor.document.is_read_only() && editor.document.display_name() == name
        }) else {
            return false;
        };
        self.buffers[index].document = Document::virtual_view(name, text);
        self.buffers[index].anchor = None;
        if follow_end {
            self.buffers[index].cursor = self.buffers[index].document.len_chars();
        } else {
            self.buffers[index].cursor = self.buffers[index]
                .cursor
                .min(self.buffers[index].document.len_chars());
        }
        true
    }

    pub fn open_recovered(
        &mut self,
        document: Document,
        cursor: usize,
        anchor: Option<usize>,
    ) -> usize {
        let mut editor = Editor::new(document);
        editor.cursor = cursor.min(editor.document.len_chars());
        editor.anchor = anchor.filter(|anchor| *anchor <= editor.document.len_chars());
        self.active = self.push_editor(editor);
        self.active
    }

    /// Append and activate a file-backed document whose canonical path the
    /// caller has already checked against every open buffer. This deliberately
    /// skips `open`'s duplicate-path scan for bounded batch admission.
    ///
    /// Panics if `document` is not file-backed. Passing a duplicate path breaks
    /// Workspace's one-buffer-per-file product invariant and is a caller bug.
    pub(crate) fn admit_prevalidated_file_document(&mut self, document: Document) -> usize {
        let path = document
            .path()
            .expect("prevalidated workspace document must be file-backed");
        if let Some(index) = self.buffers.iter().position(|editor| {
            editor
                .document
                .path()
                .is_some_and(|open_path| open_path == path)
        }) {
            self.active = index;
            return index;
        }
        if self.buffers.len() == 1
            && self.buffers[0].document.path().is_none()
            && !self.buffers[0].document.is_modified()
        {
            let _replaced = self
                .replace_editor(0, Editor::new(document))
                .expect("the initial workspace buffer exists");
            self.active = 0;
            return 0;
        }
        self.active = self.push_editor(Editor::new(document));
        self.active
    }

    pub fn activate(&mut self, index: usize) -> bool {
        if index < self.buffers.len() {
            self.active = index;
            true
        } else {
            false
        }
    }

    pub fn next_buffer(&mut self) {
        self.active = (self.active + 1) % self.buffers.len();
    }

    pub fn previous_buffer(&mut self) {
        self.active = self.active.checked_sub(1).unwrap_or(self.buffers.len() - 1);
    }

    pub fn close_active(&mut self, force: bool) -> Result<(), &'static str> {
        if self.active().document.is_modified() && !force {
            return Err("buffer has unsaved changes");
        }
        let removed = self.buffers.remove(self.active);
        self.editor_indices.remove(&removed.id());
        if self.buffers.is_empty() {
            self.active = self.push_editor(Editor::new(Document::new()));
        } else {
            self.reindex_from(self.active);
            self.active = self.active.min(self.buffers.len() - 1);
        }
        debug_assert!(self.indices_are_consistent());
        Ok(())
    }

    pub fn close_other_buffers(&mut self) -> Result<Vec<u64>, &'static str> {
        if self.buffers.len() <= 1 {
            return Ok(Vec::new());
        }
        let active_id = self.active().id();
        if self
            .buffers
            .iter()
            .any(|editor| editor.id() != active_id && editor.document.is_modified())
        {
            return Err("other buffers have unsaved changes");
        }

        let mut closed = Vec::new();
        self.buffers.retain(|editor| {
            if editor.id() == active_id {
                true
            } else {
                closed.push(editor.id());
                false
            }
        });
        for editor_id in &closed {
            self.editor_indices.remove(editor_id);
        }
        self.active = 0;
        self.reindex_from(0);
        debug_assert!(self.indices_are_consistent());
        Ok(closed)
    }

    pub fn modified_count(&self) -> usize {
        self.buffers
            .iter()
            .filter(|editor| editor.document.is_modified())
            .count()
    }

    pub fn resolve(&self, path: &Path) -> PathBuf {
        resolve_from(&self.root, path)
    }

    fn file_identity(&self, path: &Path) -> PathBuf {
        normalized_file_path(&self.resolve(path))
    }

    fn with_editor(root: PathBuf, editor: Editor) -> Self {
        let mut editor_indices = HashMap::with_capacity(1);
        editor_indices.insert(editor.id(), 0);
        Self {
            root,
            buffers: vec![editor],
            editor_indices,
            active: 0,
        }
    }

    fn push_editor(&mut self, editor: Editor) -> usize {
        let index = self.buffers.len();
        debug_assert!(
            !self.editor_indices.contains_key(&editor.id()),
            "new editor ID must be unique"
        );
        self.editor_indices.insert(editor.id(), index);
        self.buffers.push(editor);
        debug_assert!(self.indices_are_consistent());
        index
    }

    fn reindex_from(&mut self, start: usize) {
        for (index, editor) in self.buffers.iter().enumerate().skip(start) {
            self.editor_indices.insert(editor.id(), index);
        }
    }

    fn indices_are_consistent(&self) -> bool {
        self.editor_indices.len() == self.buffers.len()
            && self.buffers.iter().enumerate().all(|(index, editor)| {
                self.editor_indices.get(&editor.id()).copied() == Some(index)
            })
    }
}

pub fn discover_root(start: &Path) -> PathBuf {
    let mut current = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    if current.is_file() {
        current.pop();
    }
    for ancestor in current.ancestors() {
        if ROOT_MARKERS
            .iter()
            .any(|marker| ancestor.join(marker).exists())
        {
            return ancestor.to_path_buf();
        }
    }
    current
}

fn absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn resolve_from(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub(crate) fn normalized_file_path(path: &Path) -> PathBuf {
    // Resolve the original spelling first so paths containing both symlinks
    // and `..` retain filesystem (rather than lexical) semantics.
    if let Some(canonical) = canonicalized_with_missing_suffix(path) {
        return canonical;
    }
    let normalized = lexically_normalized(path);
    canonicalized_with_missing_suffix(&normalized).unwrap_or(normalized)
}

fn canonicalized_with_missing_suffix(path: &Path) -> Option<PathBuf> {
    // A new file cannot itself be canonicalized. Resolve its nearest existing
    // ancestor so aliases through symlinked directories still share an
    // identity, then append the missing suffix in its original order.
    let mut ancestor = path;
    let mut suffix = Vec::<OsString>::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(ancestor) {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
        let name = ancestor.file_name()?;
        suffix.push(name.to_os_string());
        ancestor = ancestor.parent()?;
    }
}

fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Workspace paths are resolved to absolute paths before this
                // helper is called, so `..` at the filesystem root is a no-op.
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_editor_index_consistency(workspace: &Workspace) {
        assert!(workspace.indices_are_consistent());
        assert_eq!(workspace.editor_indices.len(), workspace.len());
        for (index, editor) in workspace.buffers().iter().enumerate() {
            assert_eq!(workspace.editor_index(editor.id()), Some(index));
            assert_eq!(
                workspace.editor_by_id(editor.id()).map(Editor::id),
                Some(editor.id())
            );
        }
    }

    #[test]
    fn discovers_nearest_project_marker() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(
            discover_root(&nested),
            fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn opening_same_file_reuses_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, "hello").unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();
        workspace.open(&path).unwrap();
        workspace.open(&path).unwrap();
        assert_eq!(workspace.len(), 1);
    }

    #[test]
    fn refuses_to_close_dirty_buffer_without_force() {
        let mut workspace = Workspace::new(None).unwrap();
        workspace
            .active_mut()
            .insert("x", crate::EditKind::Insert)
            .unwrap();
        assert_eq!(
            workspace.close_active(false),
            Err("buffer has unsaved changes")
        );
        assert!(workspace.close_active(true).is_ok());
    }

    #[test]
    fn editor_id_lookups_cover_active_inactive_virtual_and_missing_buffers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("first.rs"), "first").unwrap();
        fs::write(dir.path().join("second.rs"), "second").unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let first = workspace.open("first.rs").unwrap();
        let first_id = workspace.buffers()[first].id();
        let second = workspace.open("second.rs").unwrap();
        let second_id = workspace.buffers()[second].id();
        let virtual_index = workspace.open_virtual("LSP Log", "ready");
        let virtual_id = workspace.buffers()[virtual_index].id();
        let active_before = workspace.active_index();

        assert_eq!(workspace.editor_index(first_id), Some(first));
        assert_eq!(workspace.editor_by_id(first_id).unwrap().id(), first_id);
        assert_eq!(workspace.editor_by_id(second_id).unwrap().id(), second_id);
        assert_eq!(workspace.editor_by_id(virtual_id).unwrap().id(), virtual_id);
        assert!(workspace.editor_index(u64::MAX).is_none());
        assert!(workspace.editor_by_id(u64::MAX).is_none());
        assert!(workspace.editor_by_id_mut(u64::MAX).is_none());

        workspace.editor_by_id_mut(first_id).unwrap().cursor = 2;
        assert_eq!(workspace.editor_by_id(first_id).unwrap().cursor, 2);
        assert_eq!(workspace.active_index(), active_before);
        assert_eq!(workspace.active().id(), virtual_id);

        let file_ids = workspace.file_editors().map(Editor::id).collect::<Vec<_>>();
        assert_eq!(file_ids, vec![first_id, second_id]);
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn editor_index_tracks_open_activate_and_first_middle_last_closes() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["first.rs", "second.rs", "third.rs", "fourth.rs"] {
            fs::write(dir.path().join(name), name).unwrap();
        }
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let mut ids = Vec::new();
        for name in ["first.rs", "second.rs", "third.rs", "fourth.rs"] {
            let index = workspace.open(name).unwrap();
            ids.push(workspace.buffers()[index].id());
            assert_editor_index_consistency(&workspace);
        }

        assert!(workspace.activate(1));
        assert_eq!(workspace.active().id(), ids[1]);
        assert_editor_index_consistency(&workspace);

        assert!(workspace.activate(0));
        workspace.close_active(false).unwrap();
        assert_eq!(
            workspace
                .buffers()
                .iter()
                .map(Editor::id)
                .collect::<Vec<_>>(),
            ids[1..]
        );
        assert!(workspace.editor_index(ids[0]).is_none());
        assert_editor_index_consistency(&workspace);

        assert!(workspace.activate(1));
        workspace.close_active(false).unwrap();
        assert_eq!(
            workspace
                .buffers()
                .iter()
                .map(Editor::id)
                .collect::<Vec<_>>(),
            vec![ids[1], ids[3]]
        );
        assert!(workspace.editor_index(ids[2]).is_none());
        assert_editor_index_consistency(&workspace);

        assert!(workspace.activate(workspace.len() - 1));
        workspace.close_active(false).unwrap();
        assert_eq!(
            workspace
                .buffers()
                .iter()
                .map(Editor::id)
                .collect::<Vec<_>>(),
            vec![ids[1]]
        );
        assert!(workspace.editor_index(ids[3]).is_none());
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn editor_index_tracks_virtual_recovered_and_whole_editor_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();
        let initial_id = workspace.active().id();

        let virtual_index = workspace.open_virtual("Problems", "first");
        let first_virtual_id = workspace.buffers()[virtual_index].id();
        assert_eq!(workspace.editor_index(initial_id), Some(0));
        assert_editor_index_consistency(&workspace);

        let refreshed_index = workspace.open_virtual("Problems", "second");
        let refreshed_virtual_id = workspace.buffers()[refreshed_index].id();
        assert_eq!(refreshed_index, virtual_index);
        assert_ne!(refreshed_virtual_id, first_virtual_id);
        assert!(workspace.editor_index(first_virtual_id).is_none());
        assert_eq!(
            workspace.editor_index(refreshed_virtual_id),
            Some(virtual_index)
        );
        assert_editor_index_consistency(&workspace);

        let recovered_index =
            workspace.open_recovered(Document::recovered(None, "recovered"), 4, Some(1));
        let recovered_id = workspace.buffers()[recovered_index].id();
        assert_eq!(workspace.editor_index(recovered_id), Some(recovered_index));
        assert_editor_index_consistency(&workspace);

        let replacement = Editor::new(Document::from_text("replacement"));
        let replacement_id = replacement.id();
        let replaced = workspace
            .replace_editor(virtual_index, replacement)
            .unwrap();
        assert_eq!(replaced.id(), refreshed_virtual_id);
        assert!(workspace.editor_index(refreshed_virtual_id).is_none());
        assert_eq!(workspace.editor_index(replacement_id), Some(virtual_index));
        assert_eq!(workspace.active_index(), recovered_index);
        assert_editor_index_consistency(&workspace);

        let duplicate_id = workspace.buffers()[recovered_index].id();
        let duplicate = workspace.buffers()[recovered_index].clone();
        assert!(workspace.replace_editor(virtual_index, duplicate).is_none());
        assert_eq!(workspace.editor_index(duplicate_id), Some(recovered_index));
        assert_eq!(workspace.editor_index(replacement_id), Some(virtual_index));
        assert_editor_index_consistency(&workspace);

        assert!(workspace.editor_index(u64::MAX).is_none());
        assert!(workspace.editor_by_id(u64::MAX).is_none());
        assert!(workspace.editor_by_id_mut(u64::MAX).is_none());
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn closing_the_only_buffer_indexes_the_fresh_fallback() {
        let mut workspace = Workspace::new(None).unwrap();
        let closed_id = workspace.active().id();

        workspace.close_active(false).unwrap();

        assert_eq!(workspace.len(), 1);
        assert_ne!(workspace.active().id(), closed_id);
        assert!(workspace.editor_index(closed_id).is_none());
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn forgotten_mutable_state_guard_cannot_stale_editor_index() {
        let mut workspace = Workspace::new(None).unwrap();
        let initial_id = workspace.active().id();
        let replacement = Editor::new(Document::from_text("replacement"));
        let replacement_id = replacement.id();
        let replacement_state = (*replacement).clone();

        let mut state = workspace.active_mut();
        *state = replacement_state;
        std::mem::forget(state);

        assert_eq!(workspace.active().document.text(), "replacement");
        assert_eq!(workspace.active().id(), initial_id);
        assert_eq!(workspace.editor_index(initial_id), Some(0));
        assert!(workspace.editor_index(replacement_id).is_none());
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn document_iteration_cannot_change_editor_identity_or_order() {
        let mut workspace = Workspace::new(None).unwrap();
        workspace.new_buffer();
        let ids = workspace
            .buffers()
            .iter()
            .map(Editor::id)
            .collect::<Vec<_>>();

        for (index, document) in workspace.documents_mut().enumerate() {
            *document = Document::from_text(&format!("document {index}"));
        }

        assert_eq!(
            workspace
                .buffers()
                .iter()
                .map(Editor::id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn prevalidated_file_document_admission_replaces_initial_and_deduplicates_exact_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepared.rs");
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let first = workspace.admit_prevalidated_file_document(Document::new_at(&path));
        let first_id = workspace.active().id();
        let duplicate = workspace.admit_prevalidated_file_document(Document::new_at(&path));

        assert_eq!(first, 0);
        assert_eq!(duplicate, first);
        assert_eq!(workspace.len(), 1);
        assert_eq!(workspace.active().id(), first_id);
        assert_eq!(workspace.active().document.path(), Some(path.as_path()));
        assert_editor_index_consistency(&workspace);
    }

    #[test]
    fn path_lookups_resolve_aliases_without_activating_the_file_buffer() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("target.rs"), "target").unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let target_index = workspace.open("target.rs").unwrap();
        let target_id = workspace.buffers()[target_index].id();
        let alias_index = workspace.open("nested/../target.rs").unwrap();
        assert_eq!(alias_index, target_index);
        assert_eq!(workspace.len(), 1);

        let virtual_index = workspace.open_virtual("Problems", "none");
        let active_before = workspace.active_index();
        assert_eq!(active_before, virtual_index);
        assert_eq!(
            workspace
                .editor_by_path("nested/../target.rs")
                .unwrap()
                .id(),
            target_id
        );
        assert_eq!(
            workspace
                .editor_by_path(dir.path().join("target.rs"))
                .unwrap()
                .id(),
            target_id
        );
        assert!(workspace.editor_by_path("missing.rs").is_none());

        workspace.editor_by_path_mut("target.rs").unwrap().cursor = 3;
        assert_eq!(workspace.editor_by_id(target_id).unwrap().cursor, 3);
        assert_eq!(workspace.active_index(), active_before);
        assert_eq!(workspace.active().document.display_name(), "Problems");
    }

    #[test]
    fn nonexistent_file_aliases_share_one_buffer_identity() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let first = workspace.open("nested/../draft.rs").unwrap();
        let editor_id = workspace.buffers()[first].id();
        let alias = workspace.open("draft.rs").unwrap();

        assert_eq!(alias, first);
        assert_eq!(workspace.len(), 1);
        assert_eq!(
            workspace.editor_by_path("draft.rs").unwrap().id(),
            editor_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_one_buffer_identity() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.rs");
        let alias = dir.path().join("alias.rs");
        fs::write(&target, "target").unwrap();
        symlink(&target, &alias).unwrap();
        let mut workspace = Workspace::new(Some(dir.path().to_path_buf())).unwrap();

        let target_index = workspace.open(&target).unwrap();
        let target_id = workspace.buffers()[target_index].id();
        assert_eq!(workspace.open(&alias).unwrap(), target_index);
        assert_eq!(workspace.len(), 1);
        assert_eq!(workspace.editor_by_path(&alias).unwrap().id(), target_id);

        let physical_parent = dir.path().join("physical");
        let physical_nested = physical_parent.join("nested");
        let directory_alias = dir.path().join("linked-directory");
        fs::create_dir_all(&physical_nested).unwrap();
        symlink(&physical_nested, &directory_alias).unwrap();
        let new_file_through_alias = directory_alias.join("../draft.rs");
        let new_file_direct = physical_parent.join("draft.rs");
        let new_file_index = workspace.open(&new_file_through_alias).unwrap();
        assert_eq!(workspace.open(&new_file_direct).unwrap(), new_file_index);
        assert_eq!(workspace.len(), 2);
    }
}
