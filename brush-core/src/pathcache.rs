//! Path cache

use crate::{error, variables};
use std::path::PathBuf;

/// A cache of paths associated with names.
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PathCache {
    /// The cache itself.
    cache: std::collections::HashMap<String, PathBuf>,
}

impl PathCache {
    /// Clears all elements from the cache.
    pub fn reset(&mut self) {
        self.cache.clear();
    }

    /// Returns the path associated with the given name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name to lookup.
    pub fn get<S: AsRef<str>>(&self, name: S) -> Option<PathBuf> {
        self.cache.get(name.as_ref()).cloned()
    }

    /// Sets the path associated with the given name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name to set.
    /// * `path` - The path to associate with the name.
    pub fn set<T: Into<String>>(&mut self, name: T, path: PathBuf) {
        self.cache.insert(name.into(), path);
    }

    /// Returns whether the cache holds no paths.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Returns the cached names and their paths, sorted by name.
    pub fn entries(&self) -> Vec<(&str, &std::path::Path)> {
        let mut entries: Vec<_> = self
            .cache
            .iter()
            .map(|(name, path)| (name.as_str(), path.as_path()))
            .collect();
        entries.sort_unstable();
        entries
    }

    /// Projects the cache into a shell value.
    pub fn to_value(&self) -> Result<variables::ShellValue, error::Error> {
        let pairs = self
            .cache
            .iter()
            .map(|(k, v)| (Some(k.to_owned()), v.to_string_lossy().to_string()))
            .collect::<Vec<_>>();

        variables::ShellValue::associative_array_from_literals(variables::ArrayLiteral(pairs))
    }

    /// Removes the path associated with the given name, if there is one.
    /// Returns whether or not an entry was removed.
    ///
    /// # Arguments
    ///
    /// * `name` - The name to remove.
    pub fn unset<S: AsRef<str>>(&mut self, name: S) -> bool {
        self.cache.remove(name.as_ref()).is_some()
    }
}
