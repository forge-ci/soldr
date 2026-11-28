//! Loading an optional shared library and looking up its symbols.
//!
//! For probing whether a runtime (a GPU driver, an inference provider) is
//! installed, and calling into it when it is: `dlopen`/`dlsym` on Unix,
//! `LoadLibraryW`/`GetProcAddress` on Windows. Library names are host
//! spellings (`libvulkan.so.1`, `vulkan-1.dll`); the caller picks them, for
//! example with [`crate::platform::executable::native_library_name`].

use std::ffi::c_void;
use std::io;

/// A loaded shared library, unloaded on drop.
pub struct Library {
    inner: crate::native_dylib::Library,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

impl Library {
    /// Load `name` through the host's normal library search.
    ///
    /// A bare name is searched; a path is loaded as given. Fails with
    /// [`io::ErrorKind::NotFound`] when the host cannot load it, carrying the
    /// host's reason (`dlerror`, or the Windows error) in the message. Loading
    /// runs the library's initialisers, so load only libraries you trust.
    pub fn open(name: impl AsRef<std::ffi::OsStr>) -> io::Result<Self> {
        crate::native_dylib::Library::open(name.as_ref()).map(|inner| Self { inner })
    }

    /// The address of the exported symbol `name`, or `None` when it has none.
    pub fn symbol_address(&self, name: &str) -> Option<*const c_void> {
        self.inner.symbol(name)
    }

    /// The exported symbol `name` as a value of type `T`, typically an
    /// `unsafe extern "C" fn` pointer.
    ///
    /// # Safety
    ///
    /// `T` must be pointer-sized and must be the symbol's real type, and the
    /// value must not be used after this [`Library`] is dropped.
    ///
    /// # Panics
    ///
    /// When `T` is not the size of a pointer.
    pub unsafe fn symbol<T: Copy>(&self, name: &str) -> Option<T> {
        assert_eq!(
            std::mem::size_of::<T>(),
            std::mem::size_of::<*const c_void>(),
            "a symbol is read as a pointer-sized value"
        );
        let address = self.symbol_address(name)?;
        // SAFETY: the sizes match (asserted above) and the caller vouches for
        // the type.
        Some(unsafe { std::mem::transmute_copy::<*const c_void, T>(&address) })
    }
}
