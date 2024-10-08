use crate::utils::resolve_links;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::{env, io};

pub trait SymLink: AsRef<Path> {
    fn is_wsl_link(&self) -> io::Result<bool>;
    fn dereference(&self) -> io::Result<Cow<'_, Path>>;

    fn resolve(&self, follow_links: bool) -> io::Result<Cow<'_, Path>> {
        if follow_links {
            self.dereference()
        } else {
            Ok(Cow::Borrowed(self.as_ref()))
        }
    }
}

/// Resolve symbolic links, including WSL links (which
/// are not handled by fs::canonicalize on Windows).
fn resolve_path(sym_path: &Path, visited: &mut HashMap<PathBuf, PathBuf>) -> io::Result<PathBuf> {
    let mut path = if sym_path.is_absolute() {
        PathBuf::new()
    } else {
        env::current_dir()?
    };

    for component in sym_path.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                path.pop();
            }
            _ => path.push(component),
        }
        let resolved = {
            if let Some(p) = visited.get(&path) {
                Cow::<'_, PathBuf>::Borrowed(p)
            } else {
                let partial_resolved = resolve_links(&path)?;
                visited.insert(path.clone(), partial_resolved.clone());

                Cow::<'_, PathBuf>::Owned(partial_resolved)
            }
        };

        if resolved.is_absolute() {
            path = resolved.into_owned();
        } else {
            path.pop();
            path.push(&*resolved);
        }

        // Recurse in case the path resolved so far contains ".."
        if visited.get(&path).is_none() {
            path = resolve_path(&path, visited)?;
        }
    }

    // Do not canonicalize here, to avoid UNC trouble
    Ok(path)
}

impl SymLink for Path {
    #[cfg(not(windows))]
    fn is_wsl_link(&self) -> io::Result<bool> {
        Ok(false)
    }

    #[cfg(windows)]
    fn is_wsl_link(&self) -> io::Result<bool> {
        use crate::utils::win;

        if !self.is_symlink() {
            Ok(false)
        } else {
            const BUF_SIZE: usize = 1024;
            let mut buf: Vec<u8> = vec![0; BUF_SIZE];
            let hdr = win::read_reparse_data::<win::ReparseHeader>(self, &mut buf)?;

            Ok(hdr.reparse_tag == win::IO_REPARSE_TAG_LX_SYMLINK)
        }
    }

    fn dereference(&self) -> io::Result<Cow<'_, Path>> {
        // map paths with possible symlink components to resolved
        let mut visited: HashMap<PathBuf, PathBuf> = HashMap::new();
        Ok(Cow::Owned(resolve_path(self, &mut visited)?))
    }
}

impl SymLink for PathBuf {
    fn is_wsl_link(&self) -> io::Result<bool> {
        self.as_path().is_wsl_link()
    }

    fn dereference(&self) -> io::Result<Cow<'_, Path>> {
        self.as_path().dereference()
    }
}
