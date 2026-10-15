//! Normalized, backend-independent virtual paths.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use crate::error::{Result, VfsError};

/// An absolute, normalized path inside a virtual file system.
///
/// Every `VPath` upholds three invariants, enforced by its constructors:
///
/// * it always starts with `/`;
/// * it never ends with `/`, except for the root itself;
/// * it contains no empty, `.` or `..` components.
///
/// Both `/` and `\` are accepted as separators on input. `..` is resolved
/// lexically and clamps at the root, so a `VPath` can never point "above" the
/// file system it addresses — the same guarantee `chroot` gives a process:
///
/// ```
/// use openvfs::VPath;
///
/// let p = VPath::new("assets/./textures/../../../etc/passwd").unwrap();
/// assert_eq!(p, "/etc/passwd"); // still *inside* the virtual root
/// ```
///
/// Paths are compared byte-wise and are therefore **case-sensitive**, even
/// when a backend (such as a Windows or macOS host directory) is not.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VPath(String);

impl VPath {
    /// The root directory, `/`.
    #[must_use]
    pub fn root() -> Self {
        Self(String::from("/"))
    }

    /// Parses and normalizes `path`. Relative input is interpreted from the root.
    ///
    /// # Errors
    ///
    /// [`VfsError::InvalidPath`] if the path contains a NUL byte.
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        let raw = path.as_ref();
        if raw.contains('\0') {
            return Err(VfsError::InvalidPath(raw.to_owned()));
        }
        if is_normalized(raw) {
            return Ok(Self(raw.to_owned())); // fast path: no re-assembly needed
        }
        let mut parts: Vec<&str> = Vec::new();
        for component in raw.split(['/', '\\']) {
            match component {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                name => parts.push(name),
            }
        }
        Ok(Self::from_components(&parts))
    }

    fn from_components(parts: &[&str]) -> Self {
        if parts.is_empty() {
            return Self::root();
        }
        let mut s = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for part in parts {
            s.push('/');
            s.push_str(part);
        }
        Self(s)
    }

    /// The path as a string slice, e.g. `"/a/b"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` for `/`.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// The parent directory, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let idx = self.0.rfind('/').unwrap_or(0); // always found: paths start with '/'
        Some(if idx == 0 { Self::root() } else { Self(self.0[..idx].to_owned()) })
    }

    /// The last component, or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() { None } else { self.0.rsplit('/').next() }
    }

    /// The extension of the last component, without the dot.
    ///
    /// Dot-files such as `.gitignore` have no extension.
    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name()?;
        let dot = name.rfind('.')?;
        (dot > 0 && dot + 1 < name.len()).then(|| &name[dot + 1..])
    }

    /// Resolves `rel` against `self`.
    ///
    /// `rel` is always treated as relative — a leading `/` does not jump back
    /// to the root. `..` may walk above `self`, but never above `/`.
    pub fn join(&self, rel: impl AsRef<str>) -> Result<Self> {
        Self::new(format!("{}/{}", self.0, rel.as_ref()))
    }

    /// Iterates over the components, root first: `/a/b` yields `"a"`, `"b"`.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// Number of components; the root has depth 0.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// Component-wise prefix test: `/a/b` starts with `/a`, but `/ab` does not.
    #[must_use]
    pub fn starts_with(&self, base: &Self) -> bool {
        base.is_root()
            || (self.0.starts_with(&base.0)
                && (self.0.len() == base.0.len() || self.0.as_bytes()[base.0.len()] == b'/'))
    }

    /// Removes `base` from the front, returning the remainder as a path rooted at `/`.
    ///
    /// `/a/b/c` stripped of `/a` is `/b/c`; a path stripped of itself is `/`.
    #[must_use]
    pub fn strip_prefix(&self, base: &Self) -> Option<Self> {
        if !self.starts_with(base) {
            return None;
        }
        if base.is_root() {
            return Some(self.clone());
        }
        let rest = &self.0[base.0.len()..];
        Some(if rest.is_empty() { Self::root() } else { Self(rest.to_owned()) })
    }

    /// `self`, its parent, its grandparent, … down to (and including) the root.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + use<> {
        std::iter::successors(Some(self.clone()), Self::parent)
    }

    /// The case-folded form of this path: the key under which `/Textures/A.dds`
    /// and `/textures/a.dds` are the same file. See [`FoldedPath`] for the
    /// exact folding rules.
    #[must_use]
    pub fn folded(&self) -> FoldedPath {
        FoldedPath { original: self.clone(), folded: self.0.to_lowercase() }
    }

    /// Case-insensitive equality, folded with [`str::to_lowercase`] exactly
    /// as [`VPath::folded`] does.
    ///
    /// ```
    /// use openvfs::VPath;
    ///
    /// let a = VPath::new("/Textures/A.dds").unwrap();
    /// let b = VPath::new("/textures/a.dds").unwrap();
    /// assert!(a != b); // plain `VPath` comparison stays case-sensitive
    /// assert!(a.eq_ignore_case(&b));
    /// ```
    #[must_use]
    pub fn eq_ignore_case(&self, other: &Self) -> bool {
        self == other || self.0.to_lowercase() == other.0.to_lowercase()
    }

    /// Appends a single, already valid component.
    pub(crate) fn child(&self, name: &str) -> Self {
        debug_assert!(
            !name.is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != "..",
            "invalid path component {name:?}"
        );
        if self.is_root() { Self(format!("/{name}")) } else { Self(format!("{}/{name}", self.0)) }
    }

    /// Glues a root-relative `tail` under `self`: `/mnt` + `/a/b` = `/mnt/a/b`.
    pub(crate) fn concat(&self, tail: &Self) -> Self {
        match (self.is_root(), tail.is_root()) {
            (_, true) => self.clone(),
            (true, false) => tail.clone(),
            (false, false) => Self(format!("{}{}", self.0, tail.0)),
        }
    }

    /// Moves `self` from under `from` to under `to`.
    pub(crate) fn rebase(&self, from: &Self, to: &Self) -> Option<Self> {
        Some(to.concat(&self.strip_prefix(from)?))
    }

    /// The string every strict descendant starts with: `/a/` for `/a`, `/` for the root.
    pub(crate) fn descendant_prefix(&self) -> String {
        if self.is_root() { String::from("/") } else { format!("{}/", self.0) }
    }

    /// A raw value used only as an ordered-map range bound. It may break the
    /// invariants (e.g. a trailing slash) and must never escape the crate.
    pub(crate) fn bound(raw: String) -> Self {
        Self(raw)
    }
}

/// `true` if `raw` already satisfies every `VPath` invariant.
fn is_normalized(raw: &str) -> bool {
    raw == "/"
        || (raw.starts_with('/')
            && !raw.ends_with('/')
            && !raw.contains('\\')
            && raw[1..].split('/').all(|c| !c.is_empty() && c != "." && c != ".."))
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0) // honors width/alignment, e.g. `{:<30}`
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl AsRef<str> for VPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Lets maps keyed by `VPath` be queried with `&str` (`map.get("/a")`).
/// Sound because `Eq`, `Ord` and `Hash` all derive from the inner string.
impl Borrow<str> for VPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<VPath> for String {
    fn from(path: VPath) -> Self {
        path.0
    }
}

impl FromStr for VPath {
    type Err = VfsError;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl TryFrom<&str> for VPath {
    type Error = VfsError;

    fn try_from(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

impl TryFrom<String> for VPath {
    type Error = VfsError;

    fn try_from(s: String) -> Result<Self> {
        Self::new(s)
    }
}

impl PartialEq<str> for VPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for VPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A [`VPath`] pre-folded for case-insensitive comparison, from [`VPath::folded`].
///
/// The folded form is [`str::to_lowercase`] applied to the whole path — a
/// Unicode-aware lowercase, not ASCII-only and not a full Unicode case-fold:
///
/// * `"/Textures/A.DDS"` and `"/textures/a.dds"` compare equal;
/// * `"Ä"` folds to `"ä"`, so `"/ÄNDER"` matches `"/änder"`;
/// * `"ß"` stays `"ß"` (it does *not* become `"ss"`), so `"/STRÄSSE"` and
///   `"/sträße"` still differ.
///
/// No character ever folds to `/`, `\`, NUL or a `.`/`..` component, so the
/// folded form keeps every `VPath` invariant — and since folding cannot move
/// a name across components, paths that fold equal differ only in letter case.
///
/// `Eq`, `Hash`, `Ord`, [`FoldedPath::as_str`] and `Borrow<str>` all use the
/// folded form (so a map keyed by `FoldedPath` can be queried with an
/// already-folded `&str`), while the original spelling is preserved for
/// display and for addressing the real file: [`fmt::Display`] prints it and
/// [`FoldedPath::path`] returns it.
///
/// ```
/// use openvfs::{FoldedPath, VPath};
/// use std::collections::BTreeMap;
///
/// let mut by_folded = BTreeMap::new();
/// by_folded.insert(VPath::new("/Textures/A.dds").unwrap().folded(), "high-res");
/// assert_eq!(by_folded.get("/textures/a.dds"), Some(&"high-res"));
/// ```
#[derive(Clone)]
pub struct FoldedPath {
    /// The path as written, for display and for addressing the real file.
    original: VPath,
    /// `original` lowercased; the one identity `Eq`/`Hash`/`Ord` compare.
    folded: String,
}

impl FoldedPath {
    /// Parses `path` as a [`VPath`], then folds it.
    ///
    /// # Errors
    ///
    /// [`VfsError::InvalidPath`] if the path contains a NUL byte.
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        Ok(VPath::new(path)?.folded())
    }

    /// The folded form — what `Eq`, `Hash`, `Ord` and `Borrow<str>` see.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.folded
    }

    /// The original spelling, as a path.
    #[must_use]
    pub fn path(&self) -> &VPath {
        &self.original
    }

    /// The original spelling, consuming the wrapper.
    #[must_use]
    pub fn into_path(self) -> VPath {
        self.original
    }
}

impl fmt::Display for FoldedPath {
    /// Prints the *original* spelling, exactly as [`VPath`]'s `Display` does.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.original, f)
    }
}

impl fmt::Debug for FoldedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FoldedPath").field(&self.folded).finish()
    }
}

impl AsRef<str> for FoldedPath {
    /// The folded form, like [`FoldedPath::as_str`].
    fn as_ref(&self) -> &str {
        &self.folded
    }
}

/// Lets maps keyed by `FoldedPath` be queried with an *already folded* `&str`.
/// Sound because `Eq`, `Ord` and `Hash` all derive from the folded string.
impl Borrow<str> for FoldedPath {
    fn borrow(&self) -> &str {
        &self.folded
    }
}

impl From<VPath> for FoldedPath {
    fn from(path: VPath) -> Self {
        path.folded()
    }
}

impl From<FoldedPath> for VPath {
    fn from(folded: FoldedPath) -> Self {
        folded.original
    }
}

/// Identity is the folded form only: `"/A"` and `"/a"` are the same key.
impl PartialEq for FoldedPath {
    fn eq(&self, other: &Self) -> bool {
        self.folded == other.folded
    }
}

impl Eq for FoldedPath {}

impl Hash for FoldedPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.folded.hash(state);
    }
}

impl Ord for FoldedPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.folded.cmp(&other.folded)
    }
}

impl PartialOrd for FoldedPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VPath {
        VPath::new(s).unwrap()
    }

    #[test]
    fn normalizes() {
        assert_eq!(p(""), "/");
        assert_eq!(p("/"), "/");
        assert_eq!(p("a/b/"), "/a/b");
        assert_eq!(p("//a///b"), "/a/b");
        assert_eq!(p("/a/./b/../c"), "/a/c");
        assert_eq!(p(r"a\b\c"), "/a/b/c");
        assert_eq!(p(r"\a/b\"), "/a/b");
    }

    #[test]
    fn dotdot_clamps_at_root() {
        assert_eq!(p("/../../.."), "/");
        assert_eq!(p("/a/../../b"), "/b");
        assert_eq!(p(".."), "/");
    }

    #[test]
    fn three_dots_is_an_ordinary_name() {
        assert_eq!(p("/.../x"), "/.../x");
        assert_eq!(p("/a/.../.."), "/a");
    }

    #[test]
    fn rejects_nul() {
        assert!(matches!(VPath::new("a\0b"), Err(VfsError::InvalidPath(_))));
    }

    #[test]
    fn fast_path_agrees_with_slow_path() {
        for raw in ["/", "/a", "/a/b", "/a b/c", "/ação/日本", "/.hidden", "/a..b"] {
            assert!(is_normalized(raw), "{raw}");
            assert_eq!(p(raw), raw);
        }
        for raw in ["", "a", "/a/", "//a", "/./a", "/a/..", r"/a\b"] {
            assert!(!is_normalized(raw), "{raw}");
        }
    }

    #[test]
    fn parent_and_name() {
        assert_eq!(p("/a/b").parent(), Some(p("/a")));
        assert_eq!(p("/a").parent(), Some(VPath::root()));
        assert_eq!(VPath::root().parent(), None);
        assert_eq!(p("/a/b.txt").file_name(), Some("b.txt"));
        assert_eq!(VPath::root().file_name(), None);
        assert_eq!(VPath::root().depth(), 0);
        assert_eq!(p("/a/b/c").depth(), 3);
    }

    #[test]
    fn extension() {
        assert_eq!(p("/a/b.tar.gz").extension(), Some("gz"));
        assert_eq!(p("/.gitignore").extension(), None);
        assert_eq!(p("/trailing.").extension(), None);
        assert_eq!(p("/none").extension(), None);
        assert_eq!(VPath::root().extension(), None);
    }

    #[test]
    fn join_is_always_relative() {
        assert_eq!(p("/a").join("b/c").unwrap(), "/a/b/c");
        assert_eq!(p("/a").join("/b").unwrap(), "/a/b");
        assert_eq!(p("/a/b").join("../c").unwrap(), "/a/c");
        assert_eq!(p("/a").join("../../..").unwrap(), "/");
        assert_eq!(VPath::root().join("x").unwrap(), "/x");
        assert_eq!(p("/a").join("").unwrap(), "/a");
    }

    #[test]
    fn prefix_is_component_wise() {
        assert!(p("/a/b").starts_with(&p("/a")));
        assert!(p("/a").starts_with(&p("/a")));
        assert!(p("/a").starts_with(&VPath::root()));
        assert!(VPath::root().starts_with(&VPath::root()));
        assert!(!p("/ab").starts_with(&p("/a")));
        assert!(!p("/a").starts_with(&p("/a/b")));
        assert_eq!(p("/a/b/c").strip_prefix(&p("/a")), Some(p("/b/c")));
        assert_eq!(p("/a").strip_prefix(&p("/a")), Some(VPath::root()));
        assert_eq!(p("/ab").strip_prefix(&p("/a")), None);
    }

    #[test]
    fn ancestors_end_at_root() {
        let all: Vec<_> = p("/a/b").ancestors().collect();
        assert_eq!(all, vec![p("/a/b"), p("/a"), VPath::root()]);
        assert_eq!(VPath::root().ancestors().count(), 1);
    }

    #[test]
    fn rebase_and_concat() {
        assert_eq!(p("/x/y/z").rebase(&p("/x"), &p("/m/n")), Some(p("/m/n/y/z")));
        assert_eq!(p("/x").rebase(&p("/x"), &p("/m")), Some(p("/m")));
        assert_eq!(p("/q").rebase(&p("/x"), &p("/m")), None);
        assert_eq!(VPath::root().concat(&p("/a")), p("/a"));
        assert_eq!(p("/a").concat(&VPath::root()), p("/a"));
    }

    #[test]
    fn descendants_sort_contiguously_after_the_bound() {
        // The ordered-map range scans in MemoryFs / OverlayFs rely on this.
        let bound = VPath::bound(p("/a").descendant_prefix());
        let mut all = [p("/a"), p("/a/x"), p("/a/x/y"), p("/a-b"), p("/a b"), p("/a0"), p("/b")];
        all.sort();
        let inside: Vec<_> = all.iter().filter(|k| **k >= bound).take_while(|k| k.starts_with(&p("/a"))).collect();
        assert_eq!(inside, [&p("/a/x"), &p("/a/x/y")]);
    }

    #[test]
    fn display_honors_padding_and_borrow_works() {
        assert_eq!(format!("[{:<5}]", p("/a")), "[/a   ]");
        let mut map = std::collections::BTreeMap::new();
        map.insert(p("/k"), 1);
        assert_eq!(map.get("/k"), Some(&1));
        assert_eq!(String::from(p("/s")), "/s");
    }

    #[test]
    fn folded_equality_ignores_case() {
        assert_eq!(p("/Textures/A.dds").folded(), p("/textures/a.dds").folded());
        assert_eq!(p("/A").folded(), FoldedPath::new("a").unwrap());
        assert_ne!(p("/a").folded(), p("/b").folded());
        assert_ne!(p("/a/x").folded(), p("/a-x").folded());
        assert!(p("/MiXeD/CaSe").eq_ignore_case(&p("/mixed/case")));
        assert!(!p("/mixed/case").eq_ignore_case(&p("/mixed/other")));
    }

    #[test]
    fn folded_keeps_the_original_spelling() {
        let folded = p("/Textures/A.DDS").folded();
        assert_eq!(folded.as_str(), "/textures/a.dds");
        assert_eq!(folded.path(), &p("/Textures/A.DDS"));
        assert_eq!(folded.to_string(), "/Textures/A.DDS");
        assert_eq!(format!("{folded:?}"), r#"FoldedPath("/textures/a.dds")"#);
        assert_eq!(folded.clone().into_path(), p("/Textures/A.DDS"));
        assert_eq!(VPath::from(p("/x/Y").folded()), "/x/Y");
    }

    #[test]
    fn folded_uses_unicode_lowercase() {
        // `str::to_lowercase`, not `to_ascii_lowercase`: Ä folds to ä.
        assert_eq!(p("/ÄNDER").folded(), p("/änder").folded());
        // …but it is not a full Unicode case-fold: ß stays ß, it does not
        // become "ss", so these two remain different paths.
        assert_ne!(p("/STRÄSSE").folded(), p("/sträße").folded());
        assert_eq!(p("/STRÄSSE").folded().as_str(), "/strässe");
    }

    #[test]
    fn folded_orders_and_hashes_by_the_folded_form() {
        use std::collections::{BTreeMap, HashMap};
        let mut sorted = [p("/b").folded(), p("/C").folded(), p("/a").folded()];
        sorted.sort();
        assert_eq!(sorted, [p("/A").folded(), p("/B").folded(), p("/c").folded()]);

        let mut map = HashMap::new();
        map.insert(p("/K").folded(), 1);
        map.insert(p("/k").folded(), 2);
        assert_eq!(map.len(), 1, "folded twins must share one slot");
        assert_eq!(map["/k"], 2, "…and are queried with the folded key");

        let mut map = BTreeMap::new();
        map.insert(p("/K").folded(), 1);
        assert_eq!(map.get("/k"), Some(&1));
    }

    #[test]
    fn folded_form_is_itself_a_valid_path() {
        for raw in ["/", "/a b/c", "/ão/日本語/FILE.TXT", "/.Hidden", "/a..b"] {
            let folded = p(raw).folded();
            assert_eq!(p(folded.as_str()), folded.as_str());
        }
    }
}
