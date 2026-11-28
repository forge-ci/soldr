//! Cfg-free descriptor-based scratch cleanup backend.

pub(crate) struct Anchor(cap_std::fs::Dir);

impl Anchor {
    pub(crate) fn open(path: &std::path::Path) -> std::io::Result<Self> {
        cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority()).map(Self)
    }

    pub(crate) fn remove(self) -> std::io::Result<()> {
        self.0.remove_open_dir_all()
    }
}
